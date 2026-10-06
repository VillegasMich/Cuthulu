//! Runtime configuration, read from `CUTHULU_*` environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Hard upper bound for log history requested per stream.
pub const MAX_LOG_TAIL: usize = 10_000;
/// Bounds for the host panel's sampling interval, in seconds.
pub const SYSTEM_SECS: std::ops::RangeInclusive<u64> = 1..=60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address the HTTP server listens on.
    pub bind: SocketAddr,
    /// Docker endpoint, `unix://…` or `tcp://…`.
    pub docker_host: String,
    /// Disable start/stop/restart.
    pub read_only: bool,
    /// Default number of log lines sent before following.
    pub log_tail: usize,
    /// Interval of the full re-list that heals missed events.
    pub reconcile_interval: Duration,
    /// Where the host's procfs is mounted (`/host/proc` in the container).
    pub proc_dir: PathBuf,
    /// Host panel sampling interval while someone is watching.
    pub system_interval: Duration,
    /// Also list the busiest processes in the host panel.
    pub system_processes: bool,
    /// Directory for state Cuthulu owns (`todos.json`).
    pub data_dir: PathBuf,
    /// tailscaled's `LocalAPI` socket; `None` (set to empty) disables the lookup.
    pub tailscale_socket: Option<PathBuf>,
    /// Explicit Tailscale admin console URL for the topbar button.
    pub tailscale_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid value `{value}` for {key}: {reason}")]
pub struct ConfigError {
    key: &'static str,
    value: String,
    reason: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 8686)),
            docker_host: "unix:///var/run/docker.sock".to_owned(),
            read_only: false,
            log_tail: 500,
            reconcile_interval: Duration::from_secs(60),
            proc_dir: PathBuf::from("/proc"),
            system_interval: Duration::from_secs(2),
            system_processes: false,
            data_dir: PathBuf::from("data"),
            tailscale_socket: Some(PathBuf::from("/var/run/tailscale/tailscaled.sock")),
            tailscale_url: None,
        }
    }
}

impl Config {
    /// Reads the configuration from the process environment.
    ///
    /// # Errors
    /// Returns an error when a variable is set to an unparsable value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Reads the configuration through `lookup`, falling back to defaults.
    ///
    /// # Errors
    /// Returns an error when a variable is set to an unparsable value.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let d = Self::default();
        let get = |key: &'static str| lookup(key).filter(|v| !v.trim().is_empty());

        Ok(Self {
            bind: parse(get("CUTHULU_BIND"), "CUTHULU_BIND", d.bind, |v| {
                v.parse()
                    .map_err(|e: std::net::AddrParseError| e.to_string())
            })?,
            docker_host: get("CUTHULU_DOCKER_HOST").unwrap_or(d.docker_host),
            read_only: parse(
                get("CUTHULU_READ_ONLY"),
                "CUTHULU_READ_ONLY",
                false,
                parse_bool,
            )?,
            log_tail: parse(
                get("CUTHULU_LOG_TAIL"),
                "CUTHULU_LOG_TAIL",
                d.log_tail,
                |v| {
                    v.parse::<usize>().map_err(|e| e.to_string()).and_then(|n| {
                        if n <= MAX_LOG_TAIL {
                            Ok(n)
                        } else {
                            Err(format!("must be at most {MAX_LOG_TAIL}"))
                        }
                    })
                },
            )?,
            reconcile_interval: parse(
                get("CUTHULU_RECONCILE_SECS"),
                "CUTHULU_RECONCILE_SECS",
                d.reconcile_interval,
                |v| match v.parse::<u64>() {
                    Ok(0) => Err("must be greater than 0".to_owned()),
                    Ok(n) => Ok(Duration::from_secs(n)),
                    Err(e) => Err(e.to_string()),
                },
            )?,
            proc_dir: get("CUTHULU_PROC_DIR").map_or(d.proc_dir, PathBuf::from),
            system_interval: parse(
                get("CUTHULU_SYSTEM_SECS"),
                "CUTHULU_SYSTEM_SECS",
                d.system_interval,
                |v| match v.parse::<u64>() {
                    Ok(n) if SYSTEM_SECS.contains(&n) => Ok(Duration::from_secs(n)),
                    Ok(_) => Err(format!(
                        "must be between {} and {}",
                        SYSTEM_SECS.start(),
                        SYSTEM_SECS.end()
                    )),
                    Err(e) => Err(e.to_string()),
                },
            )?,
            system_processes: parse(
                get("CUTHULU_SYSTEM_PROCESSES"),
                "CUTHULU_SYSTEM_PROCESSES",
                d.system_processes,
                parse_bool,
            )?,
            data_dir: get("CUTHULU_DATA_DIR").map_or(d.data_dir, PathBuf::from),
            // Unlike the others, an empty value is meaningful here: it disables.
            tailscale_socket: match lookup("CUTHULU_TAILSCALE_SOCKET") {
                None => d.tailscale_socket,
                Some(v) if v.trim().is_empty() => None,
                Some(v) => Some(PathBuf::from(v.trim())),
            },
            tailscale_url: parse(
                get("CUTHULU_TAILSCALE_URL"),
                "CUTHULU_TAILSCALE_URL",
                d.tailscale_url,
                |v| {
                    if (v.starts_with("https://") || v.starts_with("http://"))
                        && !v.chars().any(char::is_whitespace)
                    {
                        Ok(Some(v.to_owned()))
                    } else {
                        Err("must be an http(s) URL".to_owned())
                    }
                },
            )?,
        })
    }
}

fn parse<T>(
    value: Option<String>,
    key: &'static str,
    default: T,
    f: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, ConfigError> {
    match value {
        None => Ok(default),
        Some(v) => f(v.trim()).map_err(|reason| ConfigError {
            key,
            value: v,
            reason,
        }),
    }
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err("expected true or false".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn defaults_when_unset() {
        assert_eq!(from(&[]).unwrap(), Config::default());
    }

    #[test]
    fn reads_values() {
        let c = from(&[
            ("CUTHULU_BIND", "0.0.0.0:9000"),
            ("CUTHULU_READ_ONLY", "yes"),
            ("CUTHULU_LOG_TAIL", "42"),
            ("CUTHULU_RECONCILE_SECS", "5"),
            ("CUTHULU_DOCKER_HOST", "tcp://10.0.0.1:2375"),
            ("CUTHULU_DATA_DIR", "/data"),
        ])
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:9000".parse().unwrap());
        assert!(c.read_only);
        assert_eq!(c.log_tail, 42);
        assert_eq!(c.reconcile_interval, Duration::from_secs(5));
        assert_eq!(c.docker_host, "tcp://10.0.0.1:2375");
        assert_eq!(c.data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(from(&[("CUTHULU_BIND", "nope")]).is_err());
        assert!(from(&[("CUTHULU_READ_ONLY", "maybe")]).is_err());
        assert!(from(&[("CUTHULU_LOG_TAIL", "999999")]).is_err());
        assert!(from(&[("CUTHULU_RECONCILE_SECS", "0")]).is_err());
    }

    #[test]
    fn empty_values_mean_default() {
        assert_eq!(from(&[("CUTHULU_BIND", "  ")]).unwrap(), Config::default());
    }

    #[test]
    fn host_panel_settings() {
        let c = from(&[
            ("CUTHULU_PROC_DIR", "/host/proc"),
            ("CUTHULU_SYSTEM_SECS", "5"),
            ("CUTHULU_SYSTEM_PROCESSES", "true"),
        ])
        .unwrap();
        assert!(c.system_processes);
        assert!(!Config::default().system_processes, "off by default");
        assert_eq!(c.proc_dir, PathBuf::from("/host/proc"));
        assert_eq!(c.system_interval, Duration::from_secs(5));
        assert!(from(&[("CUTHULU_SYSTEM_SECS", "0")]).is_err());
        assert!(from(&[("CUTHULU_SYSTEM_SECS", "61")]).is_err());
        assert!(from(&[("CUTHULU_SYSTEM_SECS", "x")]).is_err());
    }

    #[test]
    fn tailscale_settings() {
        let d = Config::default();
        assert_eq!(
            d.tailscale_socket.as_deref(),
            Some(std::path::Path::new("/var/run/tailscale/tailscaled.sock"))
        );
        assert_eq!(d.tailscale_url, None);

        let c = from(&[
            ("CUTHULU_TAILSCALE_SOCKET", " "),
            ("CUTHULU_TAILSCALE_URL", "https://login.tailscale.com/admin"),
        ])
        .unwrap();
        assert_eq!(c.tailscale_socket, None, "empty disables");
        assert_eq!(
            c.tailscale_url.as_deref(),
            Some("https://login.tailscale.com/admin")
        );

        let c = from(&[("CUTHULU_TAILSCALE_SOCKET", "/run/ts.sock")]).unwrap();
        assert_eq!(c.tailscale_socket, Some(PathBuf::from("/run/ts.sock")));
        assert!(from(&[("CUTHULU_TAILSCALE_URL", "javascript:alert(1)")]).is_err());
        assert!(from(&[("CUTHULU_TAILSCALE_URL", "login.tailscale.com")]).is_err());
    }
}
