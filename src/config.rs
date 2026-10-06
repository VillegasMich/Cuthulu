//! Runtime configuration, read from `CUTHULU_*` environment variables.

use std::net::SocketAddr;
use std::time::Duration;

/// Hard upper bound for log history requested per stream.
pub const MAX_LOG_TAIL: usize = 10_000;

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
        ])
        .unwrap();
        assert_eq!(c.bind, "0.0.0.0:9000".parse().unwrap());
        assert!(c.read_only);
        assert_eq!(c.log_tail, 42);
        assert_eq!(c.reconcile_interval, Duration::from_secs(5));
        assert_eq!(c.docker_host, "tcp://10.0.0.1:2375");
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
}
