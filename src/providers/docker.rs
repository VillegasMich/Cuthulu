//! Docker provider, talking to the Engine API through `bollard`.
//! This is the only module allowed to use `bollard`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::models::{
    ContainerInspectResponse, ContainerStateStatusEnum, EventMessage, EventMessageTypeEnum,
    HealthStatusEnum,
};
use bollard::query_parameters::{
    EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::stream::{self, BoxStream, StreamExt, TryStreamExt};

use super::lines::LineSplitter;
use super::{Provider, ProviderError, ProviderEvent, Result};
use crate::model::{
    Action, Health, LogLine, LogOptions, LogStream, MountInfo, PortMapping, ProviderKind, Service,
    ServiceDetail, ServiceId, ServiceState,
};

/// Seconds before a request to the Docker daemon (not a stream) times out.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Parallel inspect calls during a full list.
const INSPECT_CONCURRENCY: usize = 16;
/// Label that marks Cuthulu's own container.
pub const SELF_LABEL: &str = "cuthulu.self";
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";

/// Container event actions that can change what we display.
const RELEVANT_ACTIONS: &[&str] = &[
    "create", "start", "restart", "stop", "die", "kill", "pause", "unpause", "rename", "update",
    "oom", "destroy",
];

pub struct DockerProvider {
    docker: Docker,
    /// Short container id of the container we run in, if any.
    self_hint: Option<String>,
}

impl DockerProvider {
    /// Connects to `host` (`unix://…`, `tcp://…` or `http://…`).
    ///
    /// The connection is lazy: this fails only on a malformed host or a
    /// missing socket file, not on an unreachable daemon.
    ///
    /// # Errors
    /// Returns [`ProviderError::Unavailable`] if the client cannot be built.
    pub fn connect(host: &str) -> Result<Self> {
        let docker = if host.starts_with("unix://") || host.starts_with('/') {
            Docker::connect_with_unix(host, REQUEST_TIMEOUT_SECS, API_DEFAULT_VERSION)
        } else {
            Docker::connect_with_http(host, REQUEST_TIMEOUT_SECS, API_DEFAULT_VERSION)
        }
        .map_err(|e| ProviderError::Unavailable(Box::new(e)))?;

        let in_container = Path::new("/.dockerenv").exists();
        let self_hint = in_container
            .then(|| std::env::var("HOSTNAME").ok())
            .flatten()
            .filter(|h| h.len() >= 12 && h.bytes().all(|b| b.is_ascii_hexdigit()));

        Ok(Self { docker, self_hint })
    }

    async fn inspect(
        &self,
        native: &str,
    ) -> std::result::Result<ContainerInspectResponse, DockerError> {
        self.docker.inspect_container(native, None).await
    }
}

#[async_trait]
impl Provider for DockerProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Docker
    }

    async fn list(&self) -> Result<Vec<Service>> {
        let summaries = self
            .docker
            .list_containers(Some(ListContainersOptionsBuilder::new().all(true).build()))
            .await
            .map_err(|e| map_error(e, None))?;

        let services = stream::iter(summaries.into_iter().filter_map(|s| s.id))
            .map(|id| async move { self.inspect(&id).await })
            .buffer_unordered(INSPECT_CONCURRENCY)
            .filter_map(|res| async move {
                match res {
                    Ok(resp) => Some(Ok(resp)),
                    // Removed between list and inspect.
                    Err(DockerError::DockerResponseServerError {
                        status_code: 404, ..
                    }) => None,
                    Err(e) => Some(Err(map_error(e, None))),
                }
            })
            .try_filter_map(|resp| async move {
                Ok(service_from_inspect(&resp, self.self_hint.as_deref()))
            })
            .try_collect()
            .await?;

        Ok(services)
    }

    async fn get(&self, id: &ServiceId) -> Result<Option<Service>> {
        match self.inspect(id.native()).await {
            Ok(resp) => Ok(service_from_inspect(&resp, self.self_hint.as_deref())),
            Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => Err(map_error(e, Some(id))),
        }
    }

    async fn detail(&self, id: &ServiceId) -> Result<ServiceDetail> {
        let resp = self
            .inspect(id.native())
            .await
            .map_err(|e| map_error(e, Some(id)))?;
        detail_from_inspect(&resp, self.self_hint.as_deref())
            .ok_or_else(|| ProviderError::NotFound(id.clone()))
    }

    async fn logs(
        &self,
        id: &ServiceId,
        opts: LogOptions,
    ) -> Result<BoxStream<'static, Result<LogLine>>> {
        // Fail fast with NotFound before the caller commits to streaming.
        self.inspect(id.native())
            .await
            .map_err(|e| map_error(e, Some(id)))?;

        let options = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .follow(opts.follow)
            .tail(&opts.tail.to_string())
            .build();
        let inner = self.docker.logs(id.native(), Some(options)).boxed();

        let state = LogState {
            inner,
            splitter: LineSplitter::new(true),
            pending: VecDeque::new(),
            done: false,
            id: id.clone(),
        };
        Ok(stream::unfold(state, LogState::next).boxed())
    }

    async fn act(&self, id: &ServiceId, action: Action) -> Result<()> {
        let native = id.native();
        let res = match action {
            Action::Start => self.docker.start_container(native, None).await,
            Action::Stop => self.docker.stop_container(native, None).await,
            Action::Restart => self.docker.restart_container(native, None).await,
        };
        match res {
            // 304: already in the requested state.
            Ok(())
            | Err(DockerError::DockerResponseServerError {
                status_code: 304, ..
            }) => Ok(()),
            Err(e) => Err(map_error(e, Some(id))),
        }
    }

    fn events(&self) -> BoxStream<'static, Result<ProviderEvent>> {
        let filters = HashMap::from([("type", vec!["container"])]);
        // The request is only sent when the stream is first polled, which is
        // after the caller's initial `list()`. `since` makes Docker replay
        // anything that happened in between.
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .to_string();
        let options = EventsOptionsBuilder::new()
            .since(&since)
            .filters(&filters)
            .build();
        self.docker
            .events(Some(options))
            .map_err(|e| map_error(e, None))
            .try_filter_map(|msg| async move { Ok(event_from_message(&msg)) })
            .boxed()
    }
}

struct LogState {
    inner: BoxStream<'static, std::result::Result<LogOutput, DockerError>>,
    splitter: LineSplitter,
    pending: VecDeque<LogLine>,
    done: bool,
    id: ServiceId,
}

impl LogState {
    async fn next(mut self) -> Option<(Result<LogLine>, Self)> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                return Some((Ok(line), self));
            }
            if self.done {
                return None;
            }
            match self.inner.next().await {
                Some(Ok(out)) => {
                    let (stream, bytes) = match &out {
                        LogOutput::StdErr { message } => (LogStream::Stderr, message),
                        LogOutput::StdOut { message } | LogOutput::Console { message } => {
                            (LogStream::Stdout, message)
                        }
                        LogOutput::StdIn { .. } => continue,
                    };
                    self.splitter.push(stream, bytes, &mut self.pending);
                }
                Some(Err(e)) => {
                    self.done = true;
                    let err = map_error(e, Some(&self.id));
                    return Some((Err(err), self));
                }
                None => {
                    self.done = true;
                    self.splitter.flush(&mut self.pending);
                }
            }
        }
    }
}

fn map_error(e: DockerError, id: Option<&ServiceId>) -> ProviderError {
    match (e, id) {
        (
            DockerError::DockerResponseServerError {
                status_code: 404, ..
            },
            Some(id),
        ) => ProviderError::NotFound(id.clone()),
        (
            e @ (DockerError::SocketNotFoundError(_)
            | DockerError::IOError { .. }
            | DockerError::HyperResponseError { .. }
            | DockerError::HyperLegacyError { .. }
            | DockerError::RequestTimeoutError),
            _,
        ) => ProviderError::Unavailable(Box::new(e)),
        (e, _) => ProviderError::Backend(Box::new(e)),
    }
}

fn event_from_message(msg: &EventMessage) -> Option<ProviderEvent> {
    if msg.typ != Some(EventMessageTypeEnum::CONTAINER) {
        return None;
    }
    let action = msg.action.as_deref()?;
    let native = msg.actor.as_ref()?.id.as_deref()?;
    let id = ServiceId::new(ProviderKind::Docker, native);

    if action == "destroy" {
        Some(ProviderEvent::Removed(id))
    } else if RELEVANT_ACTIONS.contains(&action) || action.starts_with("health_status") {
        Some(ProviderEvent::Changed(id))
    } else {
        None
    }
}

fn service_from_inspect(
    resp: &ContainerInspectResponse,
    self_hint: Option<&str>,
) -> Option<Service> {
    let native = resp.id.as_deref()?;
    let state = resp.state.as_ref();
    let config = resp.config.as_ref();
    let labels = config.and_then(|c| c.labels.as_ref());
    let label = |key: &str| labels.and_then(|l| l.get(key));

    let service_state = match state.and_then(|s| s.status) {
        Some(ContainerStateStatusEnum::RUNNING | ContainerStateStatusEnum::STOPPING) => {
            ServiceState::Running
        }
        Some(ContainerStateStatusEnum::RESTARTING) => ServiceState::Restarting,
        Some(ContainerStateStatusEnum::PAUSED) => ServiceState::Paused,
        Some(ContainerStateStatusEnum::CREATED) => ServiceState::Created,
        Some(ContainerStateStatusEnum::EXITED | ContainerStateStatusEnum::REMOVING) => {
            ServiceState::Stopped
        }
        Some(ContainerStateStatusEnum::DEAD) => ServiceState::Dead,
        Some(ContainerStateStatusEnum::EMPTY) | None => ServiceState::Unknown,
    };

    let health = match state.and_then(|s| s.health.as_ref()).and_then(|h| h.status) {
        Some(HealthStatusEnum::HEALTHY) => Health::Healthy,
        Some(HealthStatusEnum::UNHEALTHY) => Health::Unhealthy,
        Some(HealthStatusEnum::STARTING) => Health::Starting,
        _ => Health::None,
    };

    let exit_code = match service_state {
        ServiceState::Stopped | ServiceState::Dead => state.and_then(|s| s.exit_code),
        _ => None,
    };

    let is_self = label(SELF_LABEL).is_some_and(|v| v == "true")
        || self_hint.is_some_and(|h| native.starts_with(h));

    Some(Service {
        id: ServiceId::new(ProviderKind::Docker, native),
        provider: ProviderKind::Docker,
        name: resp.name.as_deref().map_or_else(
            || native[..native.len().min(12)].to_owned(),
            |n| n.trim_start_matches('/').to_owned(),
        ),
        image: config.and_then(|c| c.image.clone()),
        state: service_state,
        health,
        started_at: state.and_then(|s| real_time(s.started_at.as_deref())),
        finished_at: state.and_then(|s| real_time(s.finished_at.as_deref())),
        exit_code,
        ports: ports(resp),
        group: label(COMPOSE_PROJECT_LABEL).cloned(),
        is_self,
    })
}

fn detail_from_inspect(
    resp: &ContainerInspectResponse,
    self_hint: Option<&str>,
) -> Option<ServiceDetail> {
    let service = service_from_inspect(resp, self_hint)?;
    let config = resp.config.as_ref();

    let command = {
        let parts: Vec<&str> = resp
            .path
            .iter()
            .map(String::as_str)
            .chain(resp.args.iter().flatten().map(String::as_str))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    };

    let mut env_keys: Vec<String> = config
        .and_then(|c| c.env.as_ref())
        .into_iter()
        .flatten()
        .map(|kv| {
            kv.split_once('=')
                .map_or(kv.as_str(), |(k, _)| k)
                .to_owned()
        })
        .collect();
    env_keys.sort();

    let mut networks: Vec<String> = resp
        .network_settings
        .as_ref()
        .and_then(|n| n.networks.as_ref())
        .map(|n| n.keys().cloned().collect())
        .unwrap_or_default();
    networks.sort();

    let mounts = resp
        .mounts
        .iter()
        .flatten()
        .map(|m| MountInfo {
            source: m
                .source
                .clone()
                .or_else(|| m.name.clone())
                .unwrap_or_default(),
            destination: m.destination.clone().unwrap_or_default(),
            read_only: m.rw == Some(false),
        })
        .collect();

    Some(ServiceDetail {
        service,
        command,
        created_at: real_time(resp.created.as_deref()),
        restart_policy: resp
            .host_config
            .as_ref()
            .and_then(|h| h.restart_policy.as_ref())
            .and_then(|p| p.name)
            .map(|n| n.to_string())
            .filter(|n| !n.is_empty()),
        restart_count: resp.restart_count.unwrap_or(0),
        error: resp
            .state
            .as_ref()
            .and_then(|s| s.error.clone())
            .filter(|e| !e.is_empty()),
        mounts,
        networks,
        env_keys,
        labels: config
            .and_then(|c| c.labels.as_ref())
            .map_or_else(BTreeMap::new, |l| {
                l.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            }),
    })
}

/// Docker reports "never" as the zero time `0001-01-01T00:00:00Z`.
fn real_time(ts: Option<&str>) -> Option<String> {
    ts.filter(|t| !t.is_empty() && !t.starts_with("0001-"))
        .map(str::to_owned)
}

fn ports(resp: &ContainerInspectResponse) -> Vec<PortMapping> {
    let Some(map) = resp
        .network_settings
        .as_ref()
        .and_then(|n| n.ports.as_ref())
    else {
        return Vec::new();
    };
    let mut out: Vec<PortMapping> = map
        .iter()
        .filter_map(|(key, bindings)| {
            let (port, proto) = key.split_once('/').unwrap_or((key, "tcp"));
            let port: u16 = port.parse().ok()?;
            Some((port, proto, bindings))
        })
        .flat_map(|(port, proto, bindings)| {
            let unbound = PortMapping {
                container_port: port,
                protocol: proto.to_owned(),
                host_ip: None,
                host_port: None,
            };
            match bindings.as_deref() {
                None | Some([]) => vec![unbound],
                Some(bs) => bs
                    .iter()
                    .map(|b| PortMapping {
                        host_ip: b.host_ip.clone().filter(|ip| !ip.is_empty()),
                        host_port: b.host_port.as_deref().and_then(|p| p.parse().ok()),
                        ..unbound.clone()
                    })
                    .collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.container_port, &a.protocol, &a.host_ip).cmp(&(
            b.container_port,
            &b.protocol,
            &b.host_ip,
        ))
    });
    out
}

#[cfg(test)]
mod tests {
    use bollard::models::{
        ContainerConfig, ContainerState, EventActor, Health as DockerHealth, NetworkSettings,
        PortBinding,
    };

    use super::*;

    fn inspect(status: ContainerStateStatusEnum) -> ContainerInspectResponse {
        ContainerInspectResponse {
            id: Some("3f2a9c0011223344556677889900aabbccddeeff00112233445566778899aabb".into()),
            name: Some("/web".into()),
            config: Some(ContainerConfig {
                image: Some("nginx:1.27".into()),
                env: Some(vec!["PATH=/bin".into(), "SECRET=hunter2".into()]),
                labels: Some(HashMap::from([(
                    COMPOSE_PROJECT_LABEL.to_owned(),
                    "shop".to_owned(),
                )])),
                ..Default::default()
            }),
            state: Some(ContainerState {
                status: Some(status),
                exit_code: Some(137),
                started_at: Some("2026-10-05T18:00:00.5Z".into()),
                finished_at: Some("0001-01-01T00:00:00Z".into()),
                health: Some(DockerHealth {
                    status: Some(HealthStatusEnum::HEALTHY),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                ports: Some(HashMap::from([
                    (
                        "80/tcp".to_owned(),
                        Some(vec![PortBinding {
                            host_ip: Some("127.0.0.1".into()),
                            host_port: Some("8080".into()),
                        }]),
                    ),
                    ("443/tcp".to_owned(), None),
                ])),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn maps_running_container() {
        let s = service_from_inspect(&inspect(ContainerStateStatusEnum::RUNNING), None).unwrap();
        assert_eq!(s.name, "web");
        assert_eq!(s.id.native().len(), 64);
        assert_eq!(s.image.as_deref(), Some("nginx:1.27"));
        assert_eq!(s.state, ServiceState::Running);
        assert_eq!(s.health, Health::Healthy);
        assert_eq!(s.exit_code, None, "exit code only matters once stopped");
        assert_eq!(s.started_at.as_deref(), Some("2026-10-05T18:00:00.5Z"));
        assert_eq!(s.finished_at, None, "zero time means never");
        assert_eq!(s.group.as_deref(), Some("shop"));
        assert!(!s.is_self);
        assert_eq!(s.ports.len(), 2);
        assert_eq!(s.ports[0].container_port, 80);
        assert_eq!(s.ports[0].host_port, Some(8080));
        assert_eq!(s.ports[1].host_port, None);
    }

    #[test]
    fn maps_exited_container() {
        let s = service_from_inspect(&inspect(ContainerStateStatusEnum::EXITED), None).unwrap();
        assert_eq!(s.state, ServiceState::Stopped);
        assert_eq!(s.exit_code, Some(137));
    }

    #[test]
    fn detects_self_by_hostname_or_label() {
        let resp = inspect(ContainerStateStatusEnum::RUNNING);
        assert!(
            service_from_inspect(&resp, Some("3f2a9c001122"))
                .unwrap()
                .is_self
        );

        let mut resp = inspect(ContainerStateStatusEnum::RUNNING);
        resp.config
            .as_mut()
            .unwrap()
            .labels
            .as_mut()
            .unwrap()
            .insert(SELF_LABEL.into(), "true".into());
        assert!(service_from_inspect(&resp, None).unwrap().is_self);
    }

    #[test]
    fn detail_hides_env_values() {
        let d = detail_from_inspect(&inspect(ContainerStateStatusEnum::RUNNING), None).unwrap();
        assert_eq!(d.env_keys, ["PATH", "SECRET"]);
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("hunter2"));
    }

    #[test]
    fn maps_events() {
        let msg = |action: &str| EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some(action.into()),
            actor: Some(EventActor {
                id: Some("abc".into()),
                attributes: None,
            }),
            ..Default::default()
        };
        let id = ServiceId::new(ProviderKind::Docker, "abc");
        assert_eq!(
            event_from_message(&msg("die")),
            Some(ProviderEvent::Changed(id.clone()))
        );
        assert_eq!(
            event_from_message(&msg("health_status: unhealthy")),
            Some(ProviderEvent::Changed(id.clone()))
        );
        assert_eq!(
            event_from_message(&msg("destroy")),
            Some(ProviderEvent::Removed(id))
        );
        assert_eq!(event_from_message(&msg("exec_start: sh")), None);
    }
}
