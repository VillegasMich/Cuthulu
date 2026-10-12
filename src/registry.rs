//! In-memory view of every service from every provider, kept current by
//! provider event streams plus a periodic full reconcile.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::model::{
    Action, LogLine, LogOptions, ProviderKind, Service, ServiceDetail, ServiceId, Unit,
};
use crate::providers::{self, Provider, ProviderError, ProviderEvent};

/// Capacity of the change broadcast. A subscriber further behind than this
/// is told to resync instead of slowing everyone down.
const EVENT_CAPACITY: usize = 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// A change pushed to subscribers (the browser, via SSE).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
pub enum RegistryEvent {
    Upsert(Service),
    Remove(ServiceId),
    Status(ProviderStatus),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub provider: ProviderKind,
    pub connected: bool,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("service `{0}` not found")]
    NotFound(ServiceId),
    #[error("{0}")]
    Forbidden(&'static str),
    #[error(transparent)]
    Provider(ProviderError),
}

impl From<ProviderError> for RegistryError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::NotFound(id) => Self::NotFound(id),
            e => Self::Provider(e),
        }
    }
}

pub type Result<T, E = RegistryError> = std::result::Result<T, E>;

pub struct Registry {
    providers: HashMap<ProviderKind, Arc<dyn Provider>>,
    services: Mutex<Services>,
    status: Mutex<HashMap<ProviderKind, ProviderStatus>>,
    tx: broadcast::Sender<RegistryEvent>,
    /// systemd units by service name (see [`crate::units`]).
    units: HashMap<String, Unit>,
}

/// Every service, plus the placeholder rows that stand in for a service
/// stopped through its systemd unit while no service has its name.
#[derive(Default)]
struct Services {
    by_id: HashMap<ServiceId, Service>,
    /// Placeholders by name; each is in `by_id` only while no real service
    /// has that name.
    held: HashMap<String, Service>,
}

impl Services {
    fn has_real(&self, name: &str) -> bool {
        self.by_id
            .values()
            .any(|s| !s.placeholder && s.name == name)
    }

    /// Shows or hides the placeholder for `name` to match the real services.
    fn sync_held(&mut self, name: &str, tx: &broadcast::Sender<RegistryEvent>) {
        let Some(p) = self.held.get(name) else {
            return;
        };
        let shown = self.by_id.get(&p.id);
        if self.has_real(name) {
            if shown.is_some_and(|s| s.placeholder) {
                self.by_id.remove(&p.id);
                let _ = tx.send(RegistryEvent::Remove(p.id.clone()));
            }
        } else if shown.is_none() {
            self.by_id.insert(p.id.clone(), p.clone());
            let _ = tx.send(RegistryEvent::Upsert(p.clone()));
        }
    }

    /// Drops the placeholder for `name`, if any.
    fn release(&mut self, name: &str, tx: &broadcast::Sender<RegistryEvent>) {
        let Some(p) = self.held.remove(name) else {
            return;
        };
        if self.by_id.get(&p.id).is_some_and(|s| s.placeholder) {
            self.by_id.remove(&p.id);
            let _ = tx.send(RegistryEvent::Remove(p.id));
        }
    }
}

impl Registry {
    #[must_use]
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Arc<Self> {
        Self::with_units(providers, HashMap::new())
    }

    /// A registry that tags each service named in `units` with its unit.
    #[must_use]
    pub fn with_units(
        providers: Vec<Arc<dyn Provider>>,
        units: HashMap<String, Unit>,
    ) -> Arc<Self> {
        let status = providers
            .iter()
            .map(|p| {
                let s = ProviderStatus {
                    provider: p.kind(),
                    connected: false,
                    error: None,
                };
                (p.kind(), s)
            })
            .collect();
        Arc::new(Self {
            providers: providers.into_iter().map(|p| (p.kind(), p)).collect(),
            services: Mutex::default(),
            status: Mutex::new(status),
            tx: broadcast::channel(EVENT_CAPACITY).0,
            units,
        })
    }

    /// Starts one watch loop per provider. They stop when `cancel` fires.
    pub fn spawn(
        self: &Arc<Self>,
        reconcile: Duration,
        cancel: &CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        self.providers
            .values()
            .map(|p| {
                let (this, p, cancel) = (Arc::clone(self), Arc::clone(p), cancel.clone());
                tokio::spawn(async move { this.watch(p, reconcile, cancel).await })
            })
            .collect()
    }

    /// All services, running first, then by name.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Service> {
        let mut list: Vec<Service> = lock(&self.services).by_id.values().cloned().collect();
        list.sort_by(|a, b| {
            (a.state.rank(), &a.name, &a.id).cmp(&(b.state.rank(), &b.name, &b.id))
        });
        list
    }

    #[must_use]
    pub fn statuses(&self) -> Vec<ProviderStatus> {
        let mut list: Vec<ProviderStatus> = lock(&self.status).values().cloned().collect();
        list.sort_by_key(|s| s.provider.as_str());
        list
    }

    /// Subscribe to changes. Call before [`Registry::snapshot`] so nothing
    /// is missed in between; duplicate upserts are harmless.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RegistryEvent> {
        self.tx.subscribe()
    }

    #[must_use]
    pub fn get(&self, id: &ServiceId) -> Option<Service> {
        lock(&self.services).by_id.get(id).cloned()
    }

    /// Full details, read live from the provider. A placeholder has only
    /// what its row shows.
    ///
    /// # Errors
    /// [`RegistryError::NotFound`] if the service does not exist.
    pub async fn detail(&self, id: &ServiceId) -> Result<ServiceDetail> {
        if let Some(p) = self.get(id).filter(|s| s.placeholder) {
            return Ok(ServiceDetail {
                service: p,
                command: None,
                created_at: None,
                restart_policy: None,
                restart_count: 0,
                error: None,
                mounts: vec![],
                networks: vec![],
                env_keys: vec![],
                labels: std::collections::BTreeMap::new(),
            });
        }
        Ok(self.provider(id)?.detail(id).await?)
    }

    /// Log stream of one service.
    ///
    /// # Errors
    /// [`RegistryError::NotFound`] if the service does not exist.
    pub async fn logs(
        &self,
        id: &ServiceId,
        opts: LogOptions,
    ) -> Result<BoxStream<'static, providers::Result<LogLine>>> {
        // A placeholder has no container, so no logs.
        if self.get(id).is_some_and(|s| s.placeholder) {
            return Ok(futures_util::stream::empty().boxed());
        }
        Ok(self.provider(id)?.logs(id, opts).await?)
    }

    /// Runs `action` and returns the service's state afterwards.
    ///
    /// # Errors
    /// [`RegistryError::Forbidden`] when stopping Cuthulu itself,
    /// [`RegistryError::NotFound`] for unknown services.
    pub async fn act(&self, id: &ServiceId, action: Action) -> Result<Service> {
        let current = self
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?;
        if current.is_self && action == Action::Stop {
            return Err(RegistryError::Forbidden("cuthulu cannot stop itself"));
        }
        if current.placeholder {
            // Only its unit can act; the provider would look up the name.
            return Err(RegistryError::NotFound(id.clone()));
        }
        let provider = self.provider(id)?;
        info!(service = %current.name, ?action, "action requested");
        provider.act(id, action).await?;
        self.refresh(provider.as_ref(), id).await;
        self.get(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))
    }

    /// Re-reads `id` from its provider, after something outside the
    /// provider (a systemd unit) acted on it.
    pub async fn reread(&self, id: &ServiceId) {
        if self.get(id).is_some_and(|s| s.placeholder) {
            return;
        }
        if let Ok(provider) = self.provider(id) {
            self.refresh(provider.as_ref(), id).await;
        }
    }

    /// `before` as known now: the same id, else a service with its name
    /// (a re-created container comes before a placeholder), else `before`.
    #[must_use]
    pub fn current(&self, before: &Service) -> Service {
        let services = lock(&self.services);
        services
            .by_id
            .get(&before.id)
            .or_else(|| {
                services
                    .by_id
                    .values()
                    .filter(|s| s.name == before.name)
                    .min_by_key(|s| s.placeholder)
            })
            .cloned()
            .unwrap_or_else(|| before.clone())
    }

    /// Keeps `placeholder` as a stopped row for its name, shown while no
    /// service has that name, until [`Registry::release`] or until a
    /// service with that name runs ([`Registry::releases`]).
    pub fn hold(&self, mut placeholder: Service) {
        placeholder.placeholder = true;
        let mut services = lock(&self.services);
        if services
            .by_id
            .get(&placeholder.id)
            .is_some_and(|s| !s.placeholder)
        {
            warn!(id = %placeholder.id, "placeholder id taken; not shown");
            return;
        }
        let name = placeholder.name.clone();
        if services.by_id.remove(&placeholder.id).is_some() {
            // An older placeholder for the same name: replaced below.
            let _ = self.tx.send(RegistryEvent::Remove(placeholder.id.clone()));
        }
        services.held.insert(name.clone(), placeholder);
        services.sync_held(&name, &self.tx);
    }

    /// Drops the placeholder for `name`, if any.
    pub fn release(&self, name: &str) {
        lock(&self.services).release(name, &self.tx);
    }

    /// Whether seeing `service` ends the placeholder for its name: a real
    /// service of that name is up again, whoever started it.
    #[must_use]
    pub fn releases(service: &Service) -> bool {
        !service.placeholder && service.is_up()
    }

    fn tag(&self, mut service: Service) -> Service {
        service.unit = self.units.get(&service.name).cloned();
        service
    }

    fn provider(&self, id: &ServiceId) -> Result<&Arc<dyn Provider>> {
        self.providers
            .get(&id.kind())
            .ok_or_else(|| RegistryError::NotFound(id.clone()))
    }

    async fn watch(
        self: Arc<Self>,
        provider: Arc<dyn Provider>,
        every: Duration,
        cancel: CancellationToken,
    ) {
        let kind = provider.kind();
        let mut backoff = Duration::from_secs(1);

        loop {
            let reason = tokio::select! {
                () = cancel.cancelled() => return,
                reason = self.watch_once(provider.as_ref(), every, &mut backoff) => reason,
            };
            warn!(%kind, %reason, retry_in = ?backoff, "provider disconnected");
            self.set_status(kind, Some(reason));

            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Lists, then follows events until something fails. Returns why.
    async fn watch_once(
        &self,
        provider: &dyn Provider,
        every: Duration,
        backoff: &mut Duration,
    ) -> String {
        let kind = provider.kind();
        // Subscribe first so changes during the initial list are not lost.
        let mut events = provider.events();

        match provider.list().await {
            Ok(list) => {
                info!(%kind, services = list.len(), "provider connected");
                self.replace_all(kind, list);
                self.set_status(kind, None);
                *backoff = Duration::from_secs(1);
            }
            Err(e) => return e.to_string(),
        }

        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ProviderEvent::Changed(id))) => self.refresh(provider, &id).await,
                    Some(Ok(ProviderEvent::Removed(id))) => self.remove(&id),
                    Some(Err(e)) => return e.to_string(),
                    None => return "event stream ended".to_owned(),
                },
                _ = tick.tick() => match provider.list().await {
                    Ok(list) => self.replace_all(kind, list),
                    Err(e) => return e.to_string(),
                },
            }
        }
    }

    async fn refresh(&self, provider: &dyn Provider, id: &ServiceId) {
        match provider.get(id).await {
            Ok(Some(service)) => self.upsert(service),
            Ok(None) => self.remove(id),
            // The next reconcile heals this.
            Err(e) => warn!(%id, error = %e, "refresh failed"),
        }
    }

    fn upsert(&self, service: Service) {
        let service = self.tag(service);
        let mut services = lock(&self.services);
        let name = service.name.clone();
        let releases = Self::releases(&service);
        if services.by_id.get(&service.id) != Some(&service) {
            debug!(id = %service.id, state = ?service.state, "upsert");
            services.by_id.insert(service.id.clone(), service.clone());
            // No receivers is fine.
            let _ = self.tx.send(RegistryEvent::Upsert(service));
        }
        if releases {
            services.release(&name, &self.tx);
        } else {
            services.sync_held(&name, &self.tx);
        }
    }

    fn remove(&self, id: &ServiceId) {
        let mut services = lock(&self.services);
        if let Some(gone) = services.by_id.remove(id) {
            debug!(%id, "remove");
            let _ = self.tx.send(RegistryEvent::Remove(id.clone()));
            services.sync_held(&gone.name, &self.tx);
        }
    }

    /// Replaces every service of `kind` with `list`, broadcasting only the diff.
    fn replace_all(&self, kind: ProviderKind, list: Vec<Service>) {
        let mut services = lock(&self.services);
        let fresh: HashMap<ServiceId, Service> = list
            .into_iter()
            .map(|s| (s.id.clone(), self.tag(s)))
            .collect();

        let gone: Vec<ServiceId> = services
            .by_id
            .iter()
            .filter(|(id, s)| id.kind() == kind && !s.placeholder && !fresh.contains_key(id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            services.by_id.remove(&id);
            let _ = self.tx.send(RegistryEvent::Remove(id));
        }

        let mut up = Vec::new();
        for (id, service) in fresh {
            if Self::releases(&service) {
                up.push(service.name.clone());
            }
            if services.by_id.get(&id) != Some(&service) {
                services.by_id.insert(id, service.clone());
                let _ = self.tx.send(RegistryEvent::Upsert(service));
            }
        }
        for name in up {
            services.release(&name, &self.tx);
        }
        let held: Vec<String> = services.held.keys().cloned().collect();
        for name in held {
            services.sync_held(&name, &self.tx);
        }
    }

    fn set_status(&self, kind: ProviderKind, error: Option<String>) {
        let status = ProviderStatus {
            provider: kind,
            connected: error.is_none(),
            error,
        };
        let mut all = lock(&self.status);
        if all.get(&kind) != Some(&status) {
            all.insert(kind, status.clone());
            let _ = self.tx.send(RegistryEvent::Status(status));
        }
    }
}

/// The maps hold plain data that is valid after any panic, so poisoning is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    use super::*;
    use crate::model::{Health, ServiceState};

    pub(crate) fn service(native: &str, state: ServiceState) -> Service {
        Service {
            id: ServiceId::new(ProviderKind::Docker, native),
            provider: ProviderKind::Docker,
            name: native.to_owned(),
            image: Some("img:1".into()),
            state,
            health: Health::None,
            started_at: None,
            finished_at: None,
            exit_code: None,
            ports: vec![],
            group: None,
            is_self: false,
            auto_restart: false,
            unit: None,
            placeholder: false,
        }
    }

    /// In-memory provider for tests.
    #[derive(Default)]
    pub(crate) struct MockProvider {
        pub services: Mutex<Vec<Service>>,
        pub actions: Mutex<Vec<(ServiceId, Action)>>,
        pub events: Mutex<Option<mpsc::UnboundedReceiver<providers::Result<ProviderEvent>>>>,
    }

    impl MockProvider {
        pub(crate) fn with(services: Vec<Service>) -> Self {
            Self {
                services: Mutex::new(services),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Docker
        }
        async fn list(&self) -> providers::Result<Vec<Service>> {
            Ok(lock(&self.services).clone())
        }
        async fn get(&self, id: &ServiceId) -> providers::Result<Option<Service>> {
            Ok(lock(&self.services).iter().find(|s| &s.id == id).cloned())
        }
        async fn detail(&self, id: &ServiceId) -> providers::Result<ServiceDetail> {
            Err(ProviderError::NotFound(id.clone()))
        }
        async fn logs(
            &self,
            _id: &ServiceId,
            _opts: LogOptions,
        ) -> providers::Result<BoxStream<'static, providers::Result<LogLine>>> {
            Ok(futures_util::stream::empty().boxed())
        }
        async fn act(&self, id: &ServiceId, action: Action) -> providers::Result<()> {
            lock(&self.actions).push((id.clone(), action));
            let state = match action {
                Action::Start | Action::Restart => ServiceState::Running,
                Action::Stop => ServiceState::Stopped,
            };
            if let Some(s) = lock(&self.services).iter_mut().find(|s| &s.id == id) {
                s.state = state;
            }
            Ok(())
        }
        fn events(&self) -> BoxStream<'static, providers::Result<ProviderEvent>> {
            match lock(&self.events).take() {
                Some(rx) => UnboundedReceiverStream::new(rx).boxed(),
                None => futures_util::stream::pending().boxed(),
            }
        }
    }

    fn drain(rx: &mut broadcast::Receiver<RegistryEvent>) -> Vec<RegistryEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn replace_all_broadcasts_only_the_diff() {
        let reg = Registry::new(vec![Arc::new(MockProvider::default())]);
        reg.replace_all(
            ProviderKind::Docker,
            vec![
                service("a", ServiceState::Running),
                service("b", ServiceState::Running),
            ],
        );
        let mut rx = reg.subscribe();

        reg.replace_all(
            ProviderKind::Docker,
            vec![
                service("a", ServiceState::Running),
                service("c", ServiceState::Stopped),
            ],
        );
        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RegistryEvent::Remove(id) if id.native() == "b"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RegistryEvent::Upsert(s) if s.name == "c"))
        );
    }

    #[test]
    fn snapshot_puts_running_first() {
        let reg = Registry::new(vec![Arc::new(MockProvider::default())]);
        reg.replace_all(
            ProviderKind::Docker,
            vec![
                service("a-stopped", ServiceState::Stopped),
                service("z-running", ServiceState::Running),
                service("b-running", ServiceState::Running),
            ],
        );
        let names: Vec<_> = reg.snapshot().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["b-running", "z-running", "a-stopped"]);
    }

    #[tokio::test]
    async fn refuses_to_stop_itself() {
        let mut me = service("me", ServiceState::Running);
        me.is_self = true;
        let provider = Arc::new(MockProvider::with(vec![me.clone()]));
        let reg = Registry::new(vec![provider.clone()]);
        reg.replace_all(ProviderKind::Docker, vec![me.clone()]);

        let err = reg.act(&me.id, Action::Stop).await.unwrap_err();
        assert!(matches!(err, RegistryError::Forbidden(_)));
        assert!(lock(&provider.actions).is_empty());

        // Restart is allowed.
        reg.act(&me.id, Action::Restart).await.unwrap();
    }

    #[tokio::test]
    async fn act_returns_refreshed_state() {
        let web = service("web", ServiceState::Running);
        let provider = Arc::new(MockProvider::with(vec![web.clone()]));
        let reg = Registry::new(vec![provider.clone()]);
        reg.replace_all(ProviderKind::Docker, vec![web.clone()]);

        let after = reg.act(&web.id, Action::Stop).await.unwrap();
        assert_eq!(after.state, ServiceState::Stopped);
        assert_eq!(reg.get(&web.id).unwrap().state, ServiceState::Stopped);
    }

    /// A container named `name` with an id of its own, like Docker's.
    pub(crate) fn container(name: &str, id: &str, state: ServiceState) -> Service {
        Service {
            name: name.to_owned(),
            ..service(id, state)
        }
    }

    fn placeholder(name: &str) -> Service {
        Service {
            placeholder: true,
            ..service(name, ServiceState::Stopped)
        }
    }

    fn changes(sub: &mut broadcast::Receiver<RegistryEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            out.push(match ev {
                RegistryEvent::Upsert(s) => format!("+{}", s.id),
                RegistryEvent::Remove(id) => format!("-{id}"),
                RegistryEvent::Status(_) => continue,
            });
        }
        out
    }

    #[test]
    fn placeholder_shows_only_while_no_container_has_its_name() {
        let reg = Registry::new(vec![]);
        let old = container("tool", "c1", ServiceState::Running);
        reg.replace_all(ProviderKind::Docker, vec![old.clone()]);
        let mut sub = reg.subscribe();

        // Stopped through systemd: the exited container is still there.
        reg.upsert(container("tool", "c1", ServiceState::Stopped));
        reg.hold(placeholder("tool"));
        assert_eq!(changes(&mut sub), ["+docker:c1"]);
        // `--rm` removes it: the placeholder takes its place.
        reg.remove(&old.id);
        assert_eq!(changes(&mut sub), ["-docker:c1", "+docker:tool"]);
        assert!(reg.get(&placeholder("tool").id).unwrap().placeholder);
        // A reconcile keeps it.
        reg.replace_all(ProviderKind::Docker, vec![]);
        assert_eq!(changes(&mut sub), Vec::<String>::new());

        // A created (not yet running) container hides it but does not end it.
        let new = container("tool", "c2", ServiceState::Created);
        reg.upsert(new.clone());
        assert_eq!(changes(&mut sub), ["+docker:c2", "-docker:tool"]);
        reg.remove(&new.id);
        assert_eq!(changes(&mut sub), ["-docker:c2", "+docker:tool"]);

        // Running again, whoever started it: the placeholder is gone for good.
        reg.replace_all(
            ProviderKind::Docker,
            vec![container("tool", "c3", ServiceState::Running)],
        );
        assert_eq!(changes(&mut sub), ["+docker:c3", "-docker:tool"]);
        reg.replace_all(ProviderKind::Docker, vec![]);
        assert_eq!(changes(&mut sub), ["-docker:c3"]);
        assert_eq!(reg.snapshot(), vec![]);
    }

    #[test]
    fn release_drops_the_placeholder() {
        let reg = Registry::new(vec![]);
        reg.hold(placeholder("tool"));
        assert_eq!(reg.snapshot().len(), 1);
        reg.release("tool");
        assert_eq!(reg.snapshot(), vec![]);
        // Never shown again once released.
        reg.replace_all(ProviderKind::Docker, vec![]);
        assert_eq!(reg.snapshot(), vec![]);
        // A placeholder never takes a real service's id.
        let real = service("tool", ServiceState::Stopped);
        reg.replace_all(ProviderKind::Docker, vec![real.clone()]);
        reg.hold(placeholder("x"));
        reg.hold(Service {
            name: "y".into(),
            ..placeholder("tool")
        });
        assert_eq!(reg.get(&real.id), Some(real));
    }

    #[tokio::test]
    async fn placeholders_have_no_logs_and_a_bare_detail() {
        let reg = Registry::new(vec![Arc::new(MockProvider::default())]);
        let p = placeholder("tool");
        reg.hold(p.clone());
        let detail = reg.detail(&p.id).await.unwrap();
        assert_eq!(detail.service, p);
        assert_eq!(detail.command, None);
        let logs = reg
            .logs(
                &p.id,
                LogOptions {
                    tail: 10,
                    follow: true,
                },
            )
            .await;
        assert_eq!(logs.unwrap().count().await, 0);
        // The provider never acts on it (Docker would look the name up).
        assert!(matches!(
            reg.act(&p.id, Action::Start).await,
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn current_follows_the_name() {
        let reg = Registry::new(vec![]);
        let old = container("tool", "c1", ServiceState::Running);
        assert_eq!(reg.current(&old), old, "unknown: as it was");
        reg.hold(placeholder("tool"));
        assert!(reg.current(&old).placeholder);
        let new = container("tool", "c2", ServiceState::Running);
        reg.upsert(new.clone());
        assert_eq!(reg.current(&old), new);
        reg.upsert(old.clone());
        assert_eq!(reg.current(&old), old);
    }

    #[test]
    fn services_are_tagged_with_their_unit() {
        let unit = Unit {
            name: "tool".into(),
            scope: crate::model::UnitScope::User,
        };
        let reg = Registry::with_units(vec![], HashMap::from([("tool".into(), unit.clone())]));
        reg.replace_all(
            ProviderKind::Docker,
            vec![
                container("tool", "c1", ServiceState::Running),
                service("web", ServiceState::Running),
            ],
        );
        reg.upsert(container("tool", "c2", ServiceState::Running));
        let units: Vec<_> = reg
            .snapshot()
            .into_iter()
            .map(|s| (s.id.to_string(), s.unit))
            .collect();
        assert_eq!(
            units,
            [
                ("docker:c1".to_owned(), Some(unit.clone())),
                ("docker:c2".to_owned(), Some(unit)),
                ("docker:web".to_owned(), None),
            ]
        );
    }

    #[tokio::test]
    async fn act_on_unknown_service_is_not_found() {
        let reg = Registry::new(vec![Arc::new(MockProvider::default())]);
        let id = ServiceId::new(ProviderKind::Docker, "ghost");
        assert!(matches!(
            reg.act(&id, Action::Start).await,
            Err(RegistryError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn watch_applies_provider_events() {
        let (tx, rx) = mpsc::unbounded_channel();
        let provider = Arc::new(MockProvider::with(vec![service(
            "a",
            ServiceState::Running,
        )]));
        *lock(&provider.events) = Some(rx);
        let reg = Registry::new(vec![provider.clone()]);
        let mut sub = reg.subscribe();
        let cancel = CancellationToken::new();
        let handles = reg.spawn(Duration::from_secs(3600), &cancel);

        // Initial list.
        assert!(matches!(sub.recv().await.unwrap(), RegistryEvent::Upsert(s) if s.name == "a"));
        assert!(matches!(sub.recv().await.unwrap(), RegistryEvent::Status(s) if s.connected));

        // A container appears and the provider says so.
        let b = service("b", ServiceState::Running);
        lock(&provider.services).push(b.clone());
        tx.send(Ok(ProviderEvent::Changed(b.id.clone()))).unwrap();
        assert!(matches!(sub.recv().await.unwrap(), RegistryEvent::Upsert(s) if s.name == "b"));

        tx.send(Ok(ProviderEvent::Removed(b.id.clone()))).unwrap();
        assert!(matches!(sub.recv().await.unwrap(), RegistryEvent::Remove(id) if id == b.id));

        cancel.cancel();
        for h in handles {
            h.await.unwrap();
        }
    }
}
