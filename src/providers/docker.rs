//! Docker provider, talking to the Engine API through `bollard`.
//! This is the only module allowed to use `bollard`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bollard::container::AttachContainerResults;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, ContainerStateStatusEnum, EventMessage,
    EventMessageTypeEnum, HealthStatusEnum, HostConfig,
};
use bollard::query_parameters::{
    AttachContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
    EventsOptionsBuilder, ListContainersOptionsBuilder, LogsOptionsBuilder,
    RemoveContainerOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::stream::{self, BoxStream, StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;

use super::lines::LineSplitter;
use super::{
    HostControl, HostError, HostOutput, HostScript, Provider, ProviderError, ProviderEvent, Result,
};
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
/// Label of the short-lived host helper containers; they are never listed.
pub const HELPER_LABEL: &str = "cuthulu.helper";
/// Most stdout kept from a host command (an env file is far smaller).
const HELPER_STDOUT_MAX: usize = 256 * 1024;
/// Tail of stderr kept from a host command, for error messages.
const HELPER_STDERR_MAX: usize = 4 * 1024;
/// Upper bound for pulling the helper image on first use.
const HELPER_PULL_TIMEOUT: Duration = Duration::from_secs(120);
/// Prefix of the line the helper's wrapper prints last on stderr with the
/// host script's exit status. Read from the stream instead of a container
/// wait, which races with `auto_remove`.
const EXIT_MARKER: &str = "cuthulu-helper-exit=";
/// Runs the host script (`$1`) in the host's namespaces, then reports its status.
const HELPER_WRAPPER: &str =
    "nsenter -t 1 -m -u -i -n -p -- /bin/sh -c \"$1\"; echo \"cuthulu-helper-exit=$?\" >&2";

/// Container event actions that can change what we display.
const RELEVANT_ACTIONS: &[&str] = &[
    "create", "start", "restart", "stop", "die", "kill", "pause", "unpause", "rename", "update",
    "oom", "destroy",
];

pub struct DockerProvider {
    docker: Docker,
    /// Short container id of the container we run in, if any.
    self_hint: Option<String>,
    /// Image of the host helper container (needs `nsenter`).
    helper_image: String,
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

        Ok(Self {
            docker,
            self_hint,
            helper_image: crate::config::DEFAULT_HELPER_IMAGE.to_owned(),
        })
    }

    /// Uses `image` for the host helper container instead of the default.
    #[must_use]
    pub fn with_helper_image(mut self, image: String) -> Self {
        self.helper_image = image;
        self
    }

    /// Pulls the helper image unless it is already present.
    async fn ensure_helper_image(&self) -> std::result::Result<(), HostError> {
        let image = self.helper_image.as_str();
        match self.docker.inspect_image(image).await {
            Ok(_) => return Ok(()),
            Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(e) => return Err(host_error(e)),
        }
        tracing::info!(image, "pulling the host helper image");
        let pull = self
            .docker
            .create_image(
                Some(CreateImageOptionsBuilder::new().from_image(image).build()),
                None,
                None,
            )
            .try_for_each(|_| async { Ok(()) });
        let failed = |reason: String| HostError::Image {
            image: image.to_owned(),
            reason,
        };
        match tokio::time::timeout(HELPER_PULL_TIMEOUT, pull).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(failed(e.to_string())),
            Err(_) => Err(failed(format!(
                "no answer within {}s",
                HELPER_PULL_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Creates, attaches to and starts the helper `name`, feeds `stdin`, and
    /// collects its output until it exits.
    async fn run_helper(
        &self,
        name: &str,
        script: &HostScript,
        stdin: &[u8],
    ) -> std::result::Result<HostOutput, HostError> {
        let config = ContainerCreateBody {
            image: Some(self.helper_image.clone()),
            cmd: Some(vec![
                "sh".to_owned(),
                "-c".to_owned(),
                HELPER_WRAPPER.to_owned(),
                "cuthulu-helper".to_owned(),
                script.text.clone(),
            ]),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(true),
            stdin_once: Some(true),
            tty: Some(false),
            labels: Some(HashMap::from([
                (HELPER_LABEL.to_owned(), "true".to_owned()),
                ("cuthulu.helper.op".to_owned(), script.op.to_owned()),
            ])),
            host_config: Some(HostConfig {
                privileged: Some(true),
                pid_mode: Some("host".to_owned()),
                // nsenter joins the host's network namespace anyway.
                network_mode: Some("none".to_owned()),
                auto_remove: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.docker
            .create_container(
                Some(CreateContainerOptionsBuilder::new().name(name).build()),
                config,
            )
            .await
            .map_err(host_error)?;
        let AttachContainerResults {
            mut output,
            mut input,
        } = self
            .docker
            .attach_container(
                name,
                Some(
                    AttachContainerOptionsBuilder::new()
                        .stdin(true)
                        .stdout(true)
                        .stderr(true)
                        .stream(true)
                        .build(),
                ),
            )
            .await
            .map_err(host_error)?;
        self.docker
            .start_container(name, None)
            .await
            .map_err(host_error)?;

        let io = |e: std::io::Error| HostError::Helper(format!("stdin: {e}"));
        input.write_all(stdin).await.map_err(io)?;
        // Half-closes the connection: with `stdin_once` the script sees EOF.
        input.shutdown().await.map_err(io)?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = output.next().await {
            match chunk.map_err(host_error)? {
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    if stdout.len() + message.len() > HELPER_STDOUT_MAX {
                        return Err(HostError::TooMuchOutput(HELPER_STDOUT_MAX));
                    }
                    stdout.extend_from_slice(&message);
                }
                LogOutput::StdErr { message } => {
                    stderr.extend_from_slice(&message);
                    // Keep the tail (and some slack so the marker line stays whole).
                    if stderr.len() > 2 * HELPER_STDERR_MAX {
                        stderr.drain(..stderr.len() - HELPER_STDERR_MAX);
                    }
                }
                LogOutput::StdIn { .. } => {}
            }
        }
        let (status, stderr) = split_exit_marker(&String::from_utf8_lossy(&stderr))
            .ok_or_else(|| HostError::Helper("it ended without an exit status".to_owned()))?;
        Ok(HostOutput {
            status,
            stdout,
            stderr,
        })
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

#[async_trait]
impl HostControl for DockerProvider {
    async fn run(
        &self,
        script: &HostScript,
        stdin: &[u8],
        timeout: Duration,
    ) -> std::result::Result<HostOutput, HostError> {
        self.ensure_helper_image().await?;
        let name = helper_name();
        tracing::debug!(helper = %name, op = script.op, "running host command");
        let res = tokio::time::timeout(timeout, self.run_helper(&name, script, stdin)).await;
        if !matches!(res, Ok(Ok(_))) {
            // auto_remove covers a normal exit; this covers timeouts and
            // failures before or after the start. A 404 means it is gone.
            let force = RemoveContainerOptionsBuilder::new().force(true).build();
            if let Err(e) = self.docker.remove_container(&name, Some(force)).await
                && !matches!(
                    e,
                    DockerError::DockerResponseServerError {
                        status_code: 404 | 409,
                        ..
                    }
                )
            {
                tracing::warn!(helper = %name, error = %e, "cannot remove the helper container");
            }
        }
        res.map_err(|_| HostError::Timeout(timeout))?
    }
}

/// `cuthulu-helper-<16 hex digits>`, unique per call.
fn helper_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // RandomState is seeded from the OS once per process; the counter makes
    // every name in this process differ.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cuthulu-helper-{:016x}", RandomState::new().hash_one(n))
}

/// Splits the helper wrapper's exit status off the end of stderr (the host
/// script's stderr may not end with a newline, so the marker can share its line).
fn split_exit_marker(stderr: &str) -> Option<(i32, String)> {
    let at = stderr.rfind(EXIT_MARKER)?;
    let status = stderr[at + EXIT_MARKER.len()..].trim().parse().ok()?;
    Some((status, stderr[..at].trim_end().to_owned()))
}

fn host_error(e: DockerError) -> HostError {
    match e {
        e @ (DockerError::SocketNotFoundError(_)
        | DockerError::IOError { .. }
        | DockerError::HyperResponseError { .. }
        | DockerError::HyperLegacyError { .. }
        | DockerError::RequestTimeoutError) => HostError::Unavailable(Box::new(e)),
        e => HostError::Helper(e.to_string()),
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
    let actor = msg.actor.as_ref()?;
    if actor
        .attributes
        .as_ref()
        .is_some_and(|a| a.contains_key(HELPER_LABEL))
    {
        return None;
    }
    let native = actor.id.as_deref()?;
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
    if label(HELPER_LABEL).is_some() {
        return None;
    }

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

    #[test]
    fn exit_marker_is_split_off_stderr() {
        assert_eq!(
            split_exit_marker("Sorry, try again.\ncuthulu-helper-exit=12\n"),
            Some((12, "Sorry, try again.".to_owned()))
        );
        assert_eq!(
            split_exit_marker("no newlinecuthulu-helper-exit=0\n"),
            Some((0, "no newline".to_owned()))
        );
        assert_eq!(
            split_exit_marker("cuthulu-helper-exit=127"),
            Some((127, String::new()))
        );
        assert_eq!(split_exit_marker("killed"), None);
        assert_eq!(split_exit_marker("cuthulu-helper-exit=x"), None);
    }

    #[test]
    fn helper_names_are_unique() {
        let (a, b) = (helper_name(), helper_name());
        assert_ne!(a, b);
        assert!(a.starts_with("cuthulu-helper-") && a.len() == 31, "{a}");
    }

    #[test]
    fn helpers_are_hidden() {
        let mut resp = inspect(ContainerStateStatusEnum::RUNNING);
        resp.config
            .as_mut()
            .unwrap()
            .labels
            .as_mut()
            .unwrap()
            .insert(HELPER_LABEL.into(), "true".into());
        assert_eq!(service_from_inspect(&resp, None), None);
        let msg = EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some("start".into()),
            actor: Some(EventActor {
                id: Some("abc".into()),
                attributes: Some(HashMap::from([(HELPER_LABEL.into(), "true".into())])),
            }),
            ..Default::default()
        };
        assert_eq!(event_from_message(&msg), None);
    }

    /// The helper container against the real host, on throwaway targets only:
    /// a temp file under /tmp and the user unit `cuthulu-env-edit-test`
    /// (create it first, see docs/ARCHITECTURE.md#env-file-editor). Checks
    /// a wrong sudo password fails, never a real one.
    #[tokio::test]
    #[ignore = "needs Docker, systemd and the throwaway user unit cuthulu-env-edit-test"]
    #[allow(clippy::too_many_lines)] // one scenario against the real host, step by step
    async fn helper_end_to_end() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        use crate::catalog::{Entry, UnitScope};
        use crate::envedit::host::{self, HostUser, Target};

        let docker = DockerProvider::connect("unix:///var/run/docker.sock").unwrap();
        let timeout = Duration::from_secs(30);
        let me = HostUser::parse(&std::env::var("USER").unwrap()).unwrap();
        let dir = std::path::PathBuf::from(format!("/tmp/cuthulu-env-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("env");
        std::fs::write(&file, "# throwaway\nA=1\nTOKEN=old\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let target = Target::new(&Entry {
            name: "cuthulu-env-edit-test".into(),
            container: "cuthulu-env-edit-test".into(),
            unit: "cuthulu-env-edit-test".into(),
            scope: UnitScope::User,
            env_file: file.to_str().unwrap().into(),
        })
        .unwrap();
        let main_pid = || {
            let out = std::process::Command::new("systemctl")
                .args([
                    "--user",
                    "show",
                    "-p",
                    "MainPID",
                    "--value",
                    "cuthulu-env-edit-test",
                ])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };

        // Probe: owner and size, no secret.
        let out = docker
            .run(&host::probe(&target, None).unwrap(), b"", timeout)
            .await
            .unwrap();
        assert_eq!(out.status, 0, "{}", out.stderr);
        let probe = host::parse_probe(&String::from_utf8_lossy(&out.stdout)).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!((probe.uid, probe.size), (meta.uid(), meta.len()));
        assert_eq!(host::resolve_user(&probe, None), Ok(me.clone()));

        // A wrong password is refused by sudo.
        let out = docker
            .run(
                &host::read(&target, None, &me).unwrap(),
                b"not-the-password-cuthulu-test\n",
                timeout,
            )
            .await
            .unwrap();
        assert_eq!(out.status, host::EXIT_SUDO, "{}", out.stderr);
        assert!(out.stderr.contains("incorrect password"), "{}", out.stderr);

        // Write: .bak, atomic replace with mode and owner kept, user unit restarted.
        let before = main_pid();
        let out = docker
            .run(
                &host::write(&target, None, &me, false).unwrap(),
                b"# throwaway\nA=2\n",
                timeout,
            )
            .await
            .unwrap();
        assert_eq!(out.status, 0, "{}", out.stderr);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# throwaway\nA=2\n"
        );
        let after = std::fs::metadata(&file).unwrap();
        assert_eq!(
            (after.mode() & 0o7777, after.uid(), after.gid()),
            (0o640, meta.uid(), meta.gid())
        );
        assert_ne!(
            after.ino(),
            meta.ino(),
            "renamed over, not rewritten in place"
        );
        let bak = dir.join("env.bak");
        assert_eq!(
            std::fs::read_to_string(&bak).unwrap(),
            "# throwaway\nA=1\nTOKEN=old\n"
        );
        assert_eq!(std::fs::metadata(&bak).unwrap().mode() & 0o7777, 0o640);
        assert_ne!(main_pid(), before, "the unit was restarted");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no temp files left"
        );

        // Missing file, timeout.
        std::fs::remove_file(&file).unwrap();
        let out = docker
            .run(&host::probe(&target, None).unwrap(), b"", timeout)
            .await
            .unwrap();
        assert_eq!(out.status, host::EXIT_NO_FILE);
        let slow = HostScript {
            op: "sleep",
            text: "sleep 20".into(),
        };
        let err = docker
            .run(&slow, b"", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, HostError::Timeout(_)), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
        let filters = HashMap::from([("label", vec![HELPER_LABEL])]);
        let left = docker
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .unwrap();
        assert_eq!(left.len(), 0, "helpers left behind: {left:?}");
    }
}
