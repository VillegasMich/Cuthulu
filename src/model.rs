//! Provider-agnostic domain types shared by the registry, the API and the UI.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::Serialize;

/// The kind of backend a service comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Docker,
}

impl ProviderKind {
    /// Prefix used in [`ServiceId`]s.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docker => "docker",
        }
    }

    /// Inverse of [`ProviderKind::as_str`].
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "docker" => Some(Self::Docker),
            _ => None,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable id across providers: `"<provider>:<native id>"`, e.g. `"docker:3f2a9c…"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ServiceId(String);

/// Error returned when a string is not a valid [`ServiceId`].
#[derive(Debug, thiserror::Error)]
#[error("invalid service id `{0}`, expected `<provider>:<id>`")]
pub struct InvalidServiceId(String);

impl ServiceId {
    #[must_use]
    pub fn new(kind: ProviderKind, native: &str) -> Self {
        Self(format!("{kind}:{native}"))
    }

    /// The provider this id belongs to.
    #[must_use]
    pub fn kind(&self) -> ProviderKind {
        // Construction is only possible through `new` or `from_str`, both of
        // which guarantee a known prefix.
        self.0
            .split_once(':')
            .and_then(|(k, _)| ProviderKind::from_prefix(k))
            .expect("ServiceId always carries a known provider prefix")
    }

    /// The id understood by the provider itself.
    #[must_use]
    pub fn native(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, n)| n)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ServiceId {
    type Err = InvalidServiceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid_native = |n: &str| {
            !n.is_empty()
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        };
        s.split_once(':')
            .and_then(|(prefix, native)| Some((ProviderKind::from_prefix(prefix)?, native)))
            .filter(|(_, native)| valid_native(native))
            .map(|(kind, native)| Self::new(kind, native))
            .ok_or_else(|| InvalidServiceId(s.to_owned()))
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Running,
    Restarting,
    Paused,
    Created,
    Stopped,
    Dead,
    Unknown,
}

impl ServiceState {
    /// Sort rank: things that are up come first.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Restarting => 1,
            Self::Paused => 2,
            Self::Dead => 3,
            Self::Stopped => 4,
            Self::Created => 5,
            Self::Unknown => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Healthy,
    Unhealthy,
    Starting,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortMapping {
    pub container_port: u16,
    pub protocol: String,
    pub host_ip: Option<String>,
    pub host_port: Option<u16>,
}

impl fmt::Display for PortMapping {
    /// `127.0.0.1:8080->80/tcp`, or `80/tcp` when not published.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(host_port) = self.host_port {
            match self.host_ip.as_deref() {
                Some(ip) if ip.contains(':') => write!(f, "[{ip}]:{host_port}->")?,
                Some(ip) => write!(f, "{ip}:{host_port}->")?,
                None => write!(f, "{host_port}->")?,
            }
        }
        write!(f, "{}/{}", self.container_port, self.protocol)
    }
}

/// One monitored service, as shown in the dashboard list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Service {
    pub id: ServiceId,
    pub provider: ProviderKind,
    pub name: String,
    pub image: Option<String>,
    pub state: ServiceState,
    pub health: Health,
    /// RFC 3339 timestamp, `None` if never started.
    pub started_at: Option<String>,
    /// RFC 3339 timestamp, `None` if never stopped.
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub ports: Vec<PortMapping>,
    /// Logical group, e.g. the Docker Compose project.
    pub group: Option<String>,
    /// True for Cuthulu's own container.
    pub is_self: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MountInfo {
    pub source: String,
    pub destination: String,
    pub read_only: bool,
}

/// Everything known about a service, for the detail view.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceDetail {
    #[serde(flatten)]
    pub service: Service,
    pub command: Option<String>,
    pub created_at: Option<String>,
    pub restart_policy: Option<String>,
    pub restart_count: i64,
    pub error: Option<String>,
    pub mounts: Vec<MountInfo>,
    pub networks: Vec<String>,
    /// Environment variable names only. Values may hold secrets and are never sent.
    pub env_keys: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Start,
    Stop,
    Restart,
}

impl FromStr for Action {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            "restart" => Ok(Self::Restart),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLine {
    /// RFC 3339 timestamp as reported by the provider.
    pub ts: Option<String>,
    pub stream: LogStream,
    /// Plain text, escape sequences removed.
    pub text: String,
    /// Styled ranges of `text`, sorted and non-overlapping. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<LogSpan>,
}

/// A styled range of [`LogLine::text`].
///
/// Offsets are UTF-16 code units (JavaScript string indices), `end` exclusive,
/// so the client can use `text.slice(start, end)` directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogSpan {
    pub start: usize,
    pub end: usize,
    #[serde(flatten)]
    pub style: LogStyle,
    /// Set on the level keyword of an otherwise uncolored line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<LogLevel>,
}

/// Text attributes from ANSI SGR sequences.
///
/// Colors are indices into the 16-color palette (0–7 normal, 8–15 bright);
/// 256-color and truecolor values are mapped to the nearest of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // independent SGR flags, not a state machine
pub struct LogStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<u8>,
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub dim: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub underline: bool,
}

impl LogStyle {
    #[must_use]
    pub fn has_color(&self) -> bool {
        self.fg.is_some() || self.bg.is_some()
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // signature required by serde
fn is_false(b: &bool) -> bool {
    !*b
}

/// Severity recognised from a level keyword such as `ERROR` or `level=warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogOptions {
    /// Number of history lines to send before following.
    pub tail: usize,
    pub follow: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_id_round_trip() {
        let id: ServiceId = "docker:abc123".parse().unwrap();
        assert_eq!(id.kind(), ProviderKind::Docker);
        assert_eq!(id.native(), "abc123");
        assert_eq!(id.to_string(), "docker:abc123");
    }

    #[test]
    fn service_id_rejects_garbage() {
        for bad in [
            "abc",
            "docker:",
            "systemd:foo",
            "docker:../etc",
            "docker:a/b",
        ] {
            assert!(bad.parse::<ServiceId>().is_err(), "{bad} should be invalid");
        }
    }

    #[test]
    fn port_display() {
        let p = PortMapping {
            container_port: 80,
            protocol: "tcp".into(),
            host_ip: Some("127.0.0.1".into()),
            host_port: Some(8080),
        };
        assert_eq!(p.to_string(), "127.0.0.1:8080->80/tcp");
        let unpublished = PortMapping {
            host_ip: None,
            host_port: None,
            ..p
        };
        assert_eq!(unpublished.to_string(), "80/tcp");
    }

    #[test]
    fn action_parse() {
        assert_eq!("restart".parse(), Ok(Action::Restart));
        assert!("kill".parse::<Action>().is_err());
    }
}
