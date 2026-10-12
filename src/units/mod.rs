//! Start, stop and restart of catalog services through their systemd units.
//!
//! A companion from the catalog ([`crate::catalog`]) runs as a unit, often
//! `docker run --rm` with `Restart=always`: stopping its container only
//! makes systemd start a new one. So for a service whose name is a catalog
//! `CONTAINER`, the buttons run `systemctl start|stop|restart <UNIT>` on the
//! host through [`HostControl`]. A unit that is not installed falls back to
//! the container; every other failure is reported.
//!
//! A stop through here is remembered ([`stopped`]): the registry keeps a
//! placeholder row while no container of that name exists, so the service
//! can be started again from the dashboard.

pub mod script;
pub mod stopped;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use self::stopped::{Stopped, StoppedStore};
use crate::catalog::{self, Entry, UnitScope};
use crate::envedit::detail;
use crate::envedit::host::{HostUser, TargetError};
use crate::model::{Action, Health, ProviderKind, Service, ServiceId, ServiceState, Unit};
use crate::providers::{HostControl, HostError};
use crate::registry::{Registry, RegistryEvent};

/// `systemctl start` returns once the unit's process is started.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Stop and restart wait for the unit's `ExecStop` (often `docker stop
/// --time 30`) and `TimeoutStopSec`.
const STOP_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, thiserror::Error)]
pub enum UnitError {
    #[error("{0} is a user unit: set CUTHULU_HOST_USER (scripts/install.sh does)")]
    NoUser(String),
    #[error("{0}")]
    Target(#[from] TargetError),
    #[error("{0}")]
    Host(#[from] HostError),
    #[error("{0}")]
    Failed(String),
}

/// What a `systemctl` action came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// systemd did it.
    Done,
    /// The unit is not installed on the host: act on the container instead.
    NotLoaded,
}

pub struct UnitControl {
    host: Option<Arc<dyn HostControl>>,
    catalog: Vec<Entry>,
    host_user: Option<HostUser>,
    stopped: StoppedStore,
}

impl UnitControl {
    /// Control for the embedded catalog; `host` is `None` when nothing can
    /// reach the host (every service then stays on its provider).
    #[must_use]
    pub fn new(
        host: Option<Arc<dyn HostControl>>,
        host_user: Option<HostUser>,
        data_dir: &Path,
    ) -> Self {
        Self::with_catalog(host, catalog::embedded(), host_user, data_dir)
    }

    #[must_use]
    pub fn with_catalog(
        host: Option<Arc<dyn HostControl>>,
        catalog: Vec<Entry>,
        host_user: Option<HostUser>,
        data_dir: &Path,
    ) -> Self {
        Self {
            host,
            catalog,
            host_user,
            stopped: StoppedStore::open(data_dir),
        }
    }

    /// The catalog entry for the service named `name`, if systemd runs it.
    fn entry(&self, name: &str) -> Option<&Entry> {
        self.host.as_ref()?;
        catalog::find(&self.catalog, name)
    }

    /// Units by container name, for the registry to tag services with.
    #[must_use]
    pub fn units(&self) -> HashMap<String, Unit> {
        if self.host.is_none() {
            return HashMap::new();
        }
        self.catalog
            .iter()
            .map(|e| (e.container.clone(), unit_of(e)))
            .collect()
    }

    /// Runs `action` on `service`'s unit. `Ok(None)` when systemd does not
    /// run it (not in the catalog, or no host access).
    ///
    /// # Errors
    /// [`UnitError::NoUser`] without the host user a user unit needs,
    /// [`UnitError::Host`] when the helper cannot run, [`UnitError::Failed`]
    /// when `systemctl` fails.
    pub async fn act(
        &self,
        service: &Service,
        action: Action,
    ) -> Result<Option<Outcome>, UnitError> {
        let (Some(host), Some(entry)) = (self.host.as_deref(), self.entry(&service.name)) else {
            return Ok(None);
        };
        if entry.scope == UnitScope::User && self.host_user.is_none() {
            return Err(UnitError::NoUser(entry.unit.clone()));
        }
        // Cuthulu's own restart stops it: queue the job so the reply goes out.
        let no_block = service.is_self;
        let script = script::action(entry, action, self.host_user.as_ref(), no_block)?;
        let command = script::display(&entry.unit, entry.scope, action, no_block);
        let timeout = if action == Action::Start {
            START_TIMEOUT
        } else {
            STOP_TIMEOUT
        };
        info!(service = %service.name, %command, "systemd action requested");
        let out = host.run(&script, b"", timeout).await?;
        match out.status {
            0 => Ok(Some(Outcome::Done)),
            script::EXIT_NOT_LOADED => {
                info!(service = %service.name, unit = %entry.unit, "unit not installed; using the container");
                Ok(Some(Outcome::NotLoaded))
            }
            script::EXIT_NO_USER => Err(UnitError::Failed(
                "the host user does not exist on the host; check CUTHULU_HOST_USER".to_owned(),
            )),
            script::EXIT_MANAGER => Err(UnitError::Failed(format!(
                "cannot ask systemd about {}: {}",
                entry.unit,
                detail(&out.stderr)
            ))),
            script::EXIT_SYSTEMCTL => Err(UnitError::Failed(format!(
                "{command} failed: {}",
                detail(&out.stderr)
            ))),
            // 127 / 126: nsenter or systemctl is missing.
            status => Err(UnitError::Failed(format!(
                "the host command failed (exit {status}): {}",
                detail(&out.stderr)
            ))),
        }
    }

    /// Placeholder rows for every remembered stop, for startup.
    #[must_use]
    pub fn placeholders(&self) -> Vec<Service> {
        self.stopped
            .all()
            .into_iter()
            .filter_map(|(name, stopped)| {
                let entry = self.entry(&name)?;
                Some(placeholder(&name, unit_of(entry), stopped))
            })
            .collect()
    }

    /// Remembers that `service` was stopped here and returns its placeholder.
    /// A failed write is logged: the stop itself worked.
    pub async fn remember(&self, service: &Service) -> Option<Service> {
        let entry = self.entry(&service.name)?;
        let stopped = Stopped {
            image: service.image.clone(),
            group: service.group.clone(),
            stopped_at: crate::todos::now(),
        };
        if let Err(e) = self.stopped.insert(&service.name, stopped.clone()).await {
            warn!(service = %service.name, error = %e, "cannot remember the stop");
        }
        Some(placeholder(&service.name, unit_of(entry), stopped))
    }

    /// Forgets a stop: the service was started, or showed up again.
    pub async fn forget(&self, name: &str) {
        if let Err(e) = self.stopped.remove(name).await {
            warn!(service = %name, error = %e, "cannot forget the stop");
        }
    }

    /// Forgets remembered stops whose service shows up running again,
    /// whoever started it (the registry drops the placeholder row itself).
    /// Subscribes before returning, so call it before the registry starts.
    pub fn spawn(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        cancel: &CancellationToken,
    ) -> JoinHandle<()> {
        let rx = registry.subscribe();
        let (this, reg, cancel) = (Arc::clone(self), Arc::clone(registry), cancel.clone());
        tokio::spawn(async move { this.watch(reg, rx, cancel).await })
    }

    async fn watch(
        self: Arc<Self>,
        registry: Arc<Registry>,
        mut rx: broadcast::Receiver<RegistryEvent>,
        cancel: CancellationToken,
    ) {
        self.resync(&registry).await;
        loop {
            let ev = tokio::select! {
                () = cancel.cancelled() => break,
                ev = rx.recv() => ev,
            };
            match ev {
                Ok(RegistryEvent::Upsert(s)) => self.seen(&s).await,
                Ok(RegistryEvent::Remove(_) | RegistryEvent::Status(_)) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => self.resync(&registry).await,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        debug!("stopped-services loop stopped");
    }

    async fn seen(&self, s: &Service) {
        if Registry::releases(s) && self.stopped.contains(&s.name) {
            info!(service = %s.name, "running again; placeholder dropped");
            self.forget(&s.name).await;
        }
    }

    async fn resync(&self, registry: &Registry) {
        for s in registry.snapshot() {
            self.seen(&s).await;
        }
    }
}

fn unit_of(entry: &Entry) -> Unit {
    Unit {
        name: entry.unit.clone(),
        scope: entry.scope,
    }
}

/// The row shown for a catalog service stopped here while no container of
/// that name exists: `docker:<name>`, the provider and name of the container
/// it stands for. Docker's own ids are 64 hex digits, and [`Registry::hold`]
/// refuses an id a real service has.
fn placeholder(name: &str, unit: Unit, stopped: Stopped) -> Service {
    Service {
        id: ServiceId::new(ProviderKind::Docker, name),
        provider: ProviderKind::Docker,
        name: name.to_owned(),
        image: stopped.image,
        state: ServiceState::Stopped,
        health: Health::None,
        started_at: None,
        finished_at: Some(stopped.stopped_at),
        exit_code: None,
        ports: vec![],
        group: stopped.group,
        is_self: false,
        auto_restart: false,
        unit: Some(unit),
        placeholder: true,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::envedit::tests::MockHost;
    use crate::registry::tests::service;
    use crate::todos::tests::TempDir;

    /// `tool` (system unit) and `desk` (user unit) are in the catalog.
    pub(crate) fn test_catalog() -> Vec<Entry> {
        let entry = |name: &str, scope| Entry {
            name: name.into(),
            container: name.into(),
            unit: format!("{name}-unit"),
            scope,
            env_file: format!("/etc/{name}/env"),
        };
        vec![
            entry("tool", UnitScope::System),
            entry("desk", UnitScope::User),
        ]
    }

    pub(crate) fn control(
        host: &Arc<MockHost>,
        host_user: Option<&str>,
        dir: &Path,
    ) -> UnitControl {
        UnitControl::with_catalog(
            Some(Arc::clone(host) as Arc<dyn HostControl>),
            test_catalog(),
            host_user.map(|u| HostUser::parse(u).unwrap()),
            dir,
        )
    }

    #[tokio::test]
    async fn only_catalog_services_go_through_systemd() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        let units = control(&host, None, dir.path());
        let web = service("web", ServiceState::Running);
        assert_eq!(units.act(&web, Action::Stop).await.unwrap(), None);

        host.answer("stop", 0, "", "");
        let tool = service("tool", ServiceState::Running);
        assert_eq!(
            units.act(&tool, Action::Stop).await.unwrap(),
            Some(Outcome::Done)
        );
        {
            let calls = host.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert!(calls[0].0.text.contains("systemctl stop 'tool-unit'"));
            assert_eq!(calls[0].1, b"", "nothing on stdin");
        }

        let no_host = UnitControl::with_catalog(None, test_catalog(), None, dir.path());
        assert_eq!(no_host.act(&tool, Action::Stop).await.unwrap(), None);
        assert_eq!(no_host.units(), HashMap::new());
    }

    #[tokio::test]
    async fn exit_codes_become_outcomes_and_errors() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        let units = control(&host, Some("manuel"), dir.path());
        let tool = service("tool", ServiceState::Running);

        host.answer("start", script::EXIT_NOT_LOADED, "", "");
        assert_eq!(
            units.act(&tool, Action::Start).await.unwrap(),
            Some(Outcome::NotLoaded)
        );

        host.answer(
            "stop",
            script::EXIT_SYSTEMCTL,
            "",
            "Failed to stop tool-unit.service: Access denied\n",
        );
        let err = units.act(&tool, Action::Stop).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "systemctl stop tool-unit failed: Failed to stop tool-unit.service: Access denied"
        );

        host.answer(
            "restart",
            script::EXIT_MANAGER,
            "",
            "Failed to connect to bus\n",
        );
        let desk = service("desk", ServiceState::Running);
        let err = units.act(&desk, Action::Restart).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot ask systemd about desk-unit: Failed to connect to bus"
        );
    }

    #[tokio::test]
    async fn user_units_need_the_host_user() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        let desk = service("desk", ServiceState::Running);
        let err = control(&host, None, dir.path())
            .act(&desk, Action::Stop)
            .await
            .unwrap_err();
        assert!(matches!(err, UnitError::NoUser(_)), "{err}");
        assert!(err.to_string().contains("CUTHULU_HOST_USER"), "{err}");
        assert_eq!(host.ops(), Vec::<&str>::new());

        host.answer("stop", 0, "", "");
        control(&host, Some("manuel"), dir.path())
            .act(&desk, Action::Stop)
            .await
            .unwrap();
        let calls = host.calls.lock().unwrap();
        assert!(
            calls[0]
                .0
                .text
                .contains("runuser -u 'manuel' -- env XDG_RUNTIME_DIR=/run/user/\"$uid\" systemctl --user stop 'desk-unit'"),
            "{}",
            calls[0].0.text
        );
    }

    #[tokio::test]
    async fn restarting_itself_is_queued() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        host.answer("restart", 0, "", "");
        let mut me = service("tool", ServiceState::Running);
        me.is_self = true;
        control(&host, None, dir.path())
            .act(&me, Action::Restart)
            .await
            .unwrap();
        let calls = host.calls.lock().unwrap();
        assert!(
            calls[0]
                .0
                .text
                .contains("systemctl --no-block restart 'tool-unit'")
        );
    }

    #[tokio::test]
    async fn stops_are_remembered_as_placeholders() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        let units = control(&host, None, dir.path());
        let mut tool = service("tool", ServiceState::Running);
        tool.group = Some("tools".into());
        let p = units.remember(&tool).await.unwrap();
        assert!(p.placeholder);
        assert_eq!(
            (
                p.id.as_str(),
                p.state,
                p.image.as_deref(),
                p.group.as_deref()
            ),
            (
                "docker:tool",
                ServiceState::Stopped,
                Some("img:1"),
                Some("tools")
            )
        );
        assert_eq!(
            p.unit,
            Some(Unit {
                name: "tool-unit".into(),
                scope: UnitScope::System
            })
        );
        assert_eq!(
            units.remember(&service("web", ServiceState::Running)).await,
            None
        );

        // Survives a restart of Cuthulu; names outside the catalog are dropped.
        let again = control(&host, None, dir.path());
        assert_eq!(again.placeholders(), vec![p]);
        again.forget("tool").await;
        assert_eq!(control(&host, None, dir.path()).placeholders(), vec![]);
    }

    /// `systemctl` through the Docker helper against the real host, on the
    /// throwaway user unit `cuthulu-systemd-control-test` only. Create it
    /// first (see docs/ARCHITECTURE.md, "Catalog services run by systemd"):
    /// `~/.config/systemd/user/cuthulu-systemd-control-test.service` with
    /// `ExecStart=/usr/bin/sleep infinity`, then `systemctl --user start` it.
    #[tokio::test]
    #[ignore = "needs Docker, systemd and the throwaway user unit cuthulu-systemd-control-test"]
    async fn systemctl_end_to_end() {
        use crate::providers::docker::DockerProvider;

        const UNIT: &str = "cuthulu-systemd-control-test";
        let docker = DockerProvider::connect("unix:///var/run/docker.sock").unwrap();
        let me = HostUser::parse(&std::env::var("USER").unwrap()).unwrap();
        let entry = |name: &str| Entry {
            name: name.into(),
            container: name.into(),
            unit: name.into(),
            scope: UnitScope::User,
            env_file: "/nonexistent".into(),
        };
        let dir = TempDir::new();
        let units = UnitControl::with_catalog(
            Some(Arc::new(docker)),
            vec![entry(UNIT), entry("cuthulu-systemd-control-missing")],
            Some(me),
            dir.path(),
        );
        let show = |prop: &str| {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "show", "-p", prop, "--value", UNIT])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let svc = service(UNIT, ServiceState::Running);
        assert_eq!(show("ActiveState"), "active", "start the unit first");

        let act = |action| units.act(&svc, action);
        assert_eq!(act(Action::Stop).await.unwrap(), Some(Outcome::Done));
        assert_eq!(show("ActiveState"), "inactive");
        assert_eq!(act(Action::Start).await.unwrap(), Some(Outcome::Done));
        assert_eq!(show("ActiveState"), "active");
        let before = show("MainPID");
        assert_eq!(act(Action::Restart).await.unwrap(), Some(Outcome::Done));
        assert_ne!(show("MainPID"), before, "restarted");
        // Stop is not disable: the unit file state is left alone.
        let file_state = show("UnitFileState");
        assert_eq!(act(Action::Stop).await.unwrap(), Some(Outcome::Done));
        assert_eq!(show("UnitFileState"), file_state);

        let missing = service("cuthulu-systemd-control-missing", ServiceState::Running);
        assert_eq!(
            units.act(&missing, Action::Stop).await.unwrap(),
            Some(Outcome::NotLoaded)
        );
    }

    #[tokio::test]
    async fn a_running_container_forgets_the_stop() {
        let host = Arc::new(MockHost::default());
        let dir = TempDir::new();
        let units = control(&host, None, dir.path());
        units
            .remember(&service("tool", ServiceState::Running))
            .await;
        units.seen(&service("tool", ServiceState::Stopped)).await;
        assert!(
            units.stopped.contains("tool"),
            "a stopped container is not a start"
        );
        let mut p = service("tool", ServiceState::Running);
        p.placeholder = true;
        units.seen(&p).await;
        assert!(units.stopped.contains("tool"));
        units.seen(&service("tool", ServiceState::Running)).await;
        assert!(!units.stopped.contains("tool"));
    }
}
