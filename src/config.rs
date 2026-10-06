//! Runtime configuration, read from `CUTHULU_*` environment variables.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use lettre::message::Mailbox;

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
    /// Directory for state Cuthulu owns (`todos.json`, `notify.json`).
    pub data_dir: PathBuf,
    /// Email and healthcheck settings (`CUTHULU_NOTIFY_*`, `CUTHULU_SMTP_*`,
    /// `CUTHULU_HEALTHCHECK_*`).
    pub notify: NotifyConfig,
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
            notify: NotifyConfig::default(),
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
            notify: parse_notify(&get)?,
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

// ── notifications ──────────────────────────────────────────

/// Default SMTP port: implicit TLS.
pub const DEFAULT_SMTP_PORT: u16 = 465;
/// Bounds (minutes) for the healthcheck interval and the alert cooldown.
pub const NOTIFY_MINUTES: std::ops::RangeInclusive<u64> = 1..=1440;
const DEFAULT_HEALTHCHECK_MINUTES: u64 = 5;
const DEFAULT_COOLDOWN_MINUTES: u64 = 15;
/// Display name on notification emails whose sender address has none.
const EMAIL_SENDER_NAME: &str = "cuthulu";
const REDACTED: &str = "[redacted]";

/// Outgoing notifications. Both channels are `None` when unconfigured or
/// when `CUTHULU_NOTIFY_ENABLED=false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    pub email: Option<EmailConfig>,
    pub healthcheck: Option<HealthcheckConfig>,
    /// Least time between two "down" emails for the same service.
    pub cooldown: Duration,
    /// Name for this machine in email subjects; `None` = detect.
    pub host: Option<String>,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            email: None,
            healthcheck: None,
            cooldown: Duration::from_secs(DEFAULT_COOLDOWN_MINUTES * 60),
            host: None,
        }
    }
}

/// A value that must never be logged: `Debug` prints a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailConfig {
    pub smtp_host: String,
    pub smtp_port: u16,
    pub tls: SmtpTls,
    pub credentials: Option<SmtpCredentials>,
    pub from: Mailbox,
    pub to: Mailbox,
}

/// How the SMTP connection is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpTls {
    /// TLS from the first byte (port 465).
    Implicit,
    /// Plain connection upgraded with STARTTLS (port 587).
    StartTls,
    /// No encryption. Only allowed for a server on this machine (tests).
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpCredentials {
    pub username: String,
    pub password: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthcheckConfig {
    /// Ping URL. Whoever holds it can report Cuthulu as alive: kept secret.
    pub url: Secret,
    pub interval: Duration,
}

fn invalid(key: &'static str, value: &str, reason: &str) -> ConfigError {
    ConfigError {
        key,
        value: value.to_owned(),
        reason: reason.to_owned(),
    }
}

fn parse_notify(
    get: &impl Fn(&'static str) -> Option<String>,
) -> Result<NotifyConfig, ConfigError> {
    let minutes = |key: &'static str, default: u64| {
        parse(
            get(key),
            key,
            Duration::from_secs(default * 60),
            |v| match v.parse::<u64>() {
                Ok(n) if NOTIFY_MINUTES.contains(&n) => Ok(Duration::from_secs(n * 60)),
                _ => Err(format!(
                    "expected minutes between {} and {}",
                    NOTIFY_MINUTES.start(),
                    NOTIFY_MINUTES.end()
                )),
            },
        )
    };
    let cooldown = minutes("CUTHULU_NOTIFY_COOLDOWN_MINUTES", DEFAULT_COOLDOWN_MINUTES)?;
    let interval = minutes(
        "CUTHULU_HEALTHCHECK_INTERVAL_MINUTES",
        DEFAULT_HEALTHCHECK_MINUTES,
    )?;
    let host = get("CUTHULU_NOTIFY_HOST").map(|h| h.trim().to_owned());
    let enabled = parse(
        get("CUTHULU_NOTIFY_ENABLED"),
        "CUTHULU_NOTIFY_ENABLED",
        true,
        parse_bool,
    )?;
    // Validate everything even when disabled, so turning it back on cannot
    // surface a typo made long ago.
    let email = parse_email(get)?;
    let healthcheck = parse_healthcheck(get, interval)?;
    Ok(NotifyConfig {
        email: email.filter(|_| enabled),
        healthcheck: healthcheck.filter(|_| enabled),
        cooldown,
        host,
    })
}

fn parse_email(
    get: &impl Fn(&'static str) -> Option<String>,
) -> Result<Option<EmailConfig>, ConfigError> {
    let Some(smtp_host) = get("CUTHULU_SMTP_HOST").map(|h| h.trim().to_owned()) else {
        for key in [
            "CUTHULU_NOTIFY_EMAIL_TO",
            "CUTHULU_NOTIFY_EMAIL_FROM",
            "CUTHULU_SMTP_USERNAME",
            "CUTHULU_SMTP_PASSWORD",
        ] {
            if let Some(value) = get(key) {
                let shown = if key == "CUTHULU_SMTP_PASSWORD" {
                    REDACTED
                } else {
                    &value
                };
                return Err(invalid(
                    key,
                    shown,
                    "has no effect without CUTHULU_SMTP_HOST",
                ));
            }
        }
        return Ok(None);
    };

    let smtp_port = parse(
        get("CUTHULU_SMTP_PORT"),
        "CUTHULU_SMTP_PORT",
        DEFAULT_SMTP_PORT,
        |v| match v.parse::<u16>() {
            Ok(p) if p != 0 => Ok(p),
            _ => Err("expected a port number".to_owned()),
        },
    )?;

    let tls = parse_tls(
        &smtp_host,
        smtp_port,
        get("CUTHULU_SMTP_TLS")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
    )?;

    let credentials = match (get("CUTHULU_SMTP_USERNAME"), get("CUTHULU_SMTP_PASSWORD")) {
        (Some(username), Some(password)) => Some(SmtpCredentials {
            username: username.trim().to_owned(),
            password: Secret::new(password),
        }),
        (None, None) => None,
        (Some(username), None) => {
            return Err(invalid(
                "CUTHULU_SMTP_USERNAME",
                &username,
                "CUTHULU_SMTP_PASSWORD is not set",
            ));
        }
        (None, Some(_)) => {
            return Err(invalid(
                "CUTHULU_SMTP_PASSWORD",
                REDACTED,
                "CUTHULU_SMTP_USERNAME is not set",
            ));
        }
    };

    let from = match get("CUTHULU_NOTIFY_EMAIL_FROM") {
        Some(value) => parse_mailbox("CUTHULU_NOTIFY_EMAIL_FROM", &value)?,
        None => credentials
            .as_ref()
            .and_then(|c| c.username.parse::<Mailbox>().ok())
            .ok_or_else(|| {
                invalid(
                    "CUTHULU_NOTIFY_EMAIL_FROM",
                    "",
                    "required when CUTHULU_SMTP_USERNAME is not an email address",
                )
            })?,
    };
    let from = match from.name {
        Some(_) => from,
        None => Mailbox::new(Some(EMAIL_SENDER_NAME.to_owned()), from.email),
    };
    let to = match get("CUTHULU_NOTIFY_EMAIL_TO") {
        Some(value) => parse_mailbox("CUTHULU_NOTIFY_EMAIL_TO", &value)?,
        None => Mailbox::new(None, from.email.clone()),
    };

    Ok(Some(EmailConfig {
        smtp_host,
        smtp_port,
        tls,
        credentials,
        from,
        to,
    }))
}

/// Port 465 means implicit TLS, any other STARTTLS, unless set explicitly.
fn parse_tls(host: &str, port: u16, value: Option<&str>) -> Result<SmtpTls, ConfigError> {
    match value {
        None if port == 465 => Ok(SmtpTls::Implicit),
        Some("implicit") => Ok(SmtpTls::Implicit),
        None | Some("starttls") => Ok(SmtpTls::StartTls),
        Some("none") if is_loopback(host) => Ok(SmtpTls::None),
        Some("none") => Err(invalid(
            "CUTHULU_SMTP_TLS",
            "none",
            "only allowed when CUTHULU_SMTP_HOST is this machine (localhost, 127.0.0.1, ::1)",
        )),
        Some(other) => Err(invalid(
            "CUTHULU_SMTP_TLS",
            other,
            "expected implicit, starttls or none",
        )),
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn parse_mailbox(key: &'static str, value: &str) -> Result<Mailbox, ConfigError> {
    value
        .trim()
        .parse()
        .map_err(|_| invalid(key, value, "expected an email address"))
}

fn parse_healthcheck(
    get: &impl Fn(&'static str) -> Option<String>,
    interval: Duration,
) -> Result<Option<HealthcheckConfig>, ConfigError> {
    let Some(url) = get("CUTHULU_HEALTHCHECK_URL").map(|u| u.trim().to_owned()) else {
        return Ok(None);
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        // The URL is secret: never echo it back.
        return Err(invalid(
            "CUTHULU_HEALTHCHECK_URL",
            REDACTED,
            "expected an http(s) URL, e.g. https://hc-ping.com/<uuid>",
        ));
    }
    Ok(Some(HealthcheckConfig {
        url: Secret::new(url),
        interval,
    }))
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

    const SMTP: &[(&str, &str)] = &[
        ("CUTHULU_SMTP_HOST", "smtp.gmail.com"),
        ("CUTHULU_SMTP_USERNAME", "me@gmail.com"),
        ("CUTHULU_SMTP_PASSWORD", "app-password"),
    ];

    fn with(
        base: &[(&'static str, &'static str)],
        extra: &[(&'static str, &'static str)],
    ) -> Result<Config, ConfigError> {
        let all: Vec<_> = base.iter().chain(extra).copied().collect();
        from(&all)
    }

    #[test]
    fn notifications_off_when_unconfigured() {
        let n = from(&[]).unwrap().notify;
        assert_eq!(n, NotifyConfig::default());
        assert!(n.email.is_none() && n.healthcheck.is_none());
        assert_eq!(n.cooldown, Duration::from_secs(15 * 60));
    }

    #[test]
    fn email_defaults_follow_the_reference() {
        let e = from(SMTP).unwrap().notify.email.unwrap();
        assert_eq!(e.smtp_port, 465);
        assert_eq!(e.tls, SmtpTls::Implicit);
        assert_eq!(e.from.to_string(), "cuthulu <me@gmail.com>");
        assert_eq!(e.to.to_string(), "me@gmail.com");
        assert_eq!(e.credentials.unwrap().password.expose(), "app-password");

        let e = with(
            SMTP,
            &[
                ("CUTHULU_SMTP_PORT", "587"),
                ("CUTHULU_NOTIFY_EMAIL_FROM", "Box <box@example.com>"),
                ("CUTHULU_NOTIFY_EMAIL_TO", "ops@example.com"),
            ],
        )
        .unwrap()
        .notify
        .email
        .unwrap();
        assert_eq!(e.tls, SmtpTls::StartTls);
        assert_eq!(e.from.to_string(), "Box <box@example.com>");
        assert_eq!(e.to.to_string(), "ops@example.com");
    }

    #[test]
    fn plain_smtp_only_on_loopback() {
        let local = from(&[
            ("CUTHULU_SMTP_HOST", "127.0.0.1"),
            ("CUTHULU_SMTP_PORT", "2525"),
            ("CUTHULU_SMTP_TLS", "none"),
            ("CUTHULU_NOTIFY_EMAIL_FROM", "a@example.com"),
        ])
        .unwrap();
        let e = local.notify.email.unwrap();
        assert_eq!(e.tls, SmtpTls::None);
        assert!(e.credentials.is_none());
        assert!(with(SMTP, &[("CUTHULU_SMTP_TLS", "none")]).is_err());
        assert!(with(SMTP, &[("CUTHULU_SMTP_TLS", "ssl")]).is_err());
    }

    #[test]
    fn rejects_incomplete_email_settings() {
        // Settings that would silently do nothing.
        assert!(from(&[("CUTHULU_NOTIFY_EMAIL_TO", "a@example.com")]).is_err());
        assert!(
            from(&[("CUTHULU_SMTP_HOST", "smtp.example.com")]).is_err(),
            "no sender"
        );
        assert!(
            from(&[
                ("CUTHULU_SMTP_HOST", "smtp.example.com"),
                ("CUTHULU_SMTP_USERNAME", "me@example.com"),
            ])
            .is_err(),
            "no password"
        );
        assert!(with(SMTP, &[("CUTHULU_SMTP_PORT", "0")]).is_err());
        assert!(with(SMTP, &[("CUTHULU_NOTIFY_EMAIL_TO", "not an address")]).is_err());
    }

    #[test]
    fn secrets_never_show_in_errors_or_debug() {
        let err = from(&[("CUTHULU_SMTP_PASSWORD", "hunter2")]).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        let err = from(&[("CUTHULU_HEALTHCHECK_URL", "hc-ping.com/secret-uuid")]).unwrap_err();
        assert!(!err.to_string().contains("secret-uuid"), "{err}");

        let c = with(
            SMTP,
            &[("CUTHULU_HEALTHCHECK_URL", "https://hc-ping.com/secret-uuid")],
        )
        .unwrap();
        let debug = format!("{c:?}");
        assert!(
            !debug.contains("app-password") && !debug.contains("secret-uuid"),
            "{debug}"
        );
    }

    #[test]
    fn healthcheck_settings() {
        let h = from(&[("CUTHULU_HEALTHCHECK_URL", "https://hc-ping.com/abc")])
            .unwrap()
            .notify
            .healthcheck
            .unwrap();
        assert_eq!(h.url.expose(), "https://hc-ping.com/abc");
        assert_eq!(h.interval, Duration::from_secs(300));

        let c = from(&[
            ("CUTHULU_HEALTHCHECK_URL", "http://127.0.0.1:9/ping"),
            ("CUTHULU_HEALTHCHECK_INTERVAL_MINUTES", "1"),
            ("CUTHULU_NOTIFY_COOLDOWN_MINUTES", "60"),
            ("CUTHULU_NOTIFY_HOST", "home-server"),
        ])
        .unwrap();
        assert_eq!(
            c.notify.healthcheck.unwrap().interval,
            Duration::from_secs(60)
        );
        assert_eq!(c.notify.cooldown, Duration::from_secs(3600));
        assert_eq!(c.notify.host.as_deref(), Some("home-server"));
        for bad in ["0", "1441", "x"] {
            assert!(
                from(&[("CUTHULU_HEALTHCHECK_INTERVAL_MINUTES", bad)]).is_err(),
                "{bad}"
            );
            assert!(
                from(&[("CUTHULU_NOTIFY_COOLDOWN_MINUTES", bad)]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn master_switch_turns_everything_off_but_still_validates() {
        let c = with(
            SMTP,
            &[
                ("CUTHULU_HEALTHCHECK_URL", "https://hc-ping.com/abc"),
                ("CUTHULU_NOTIFY_ENABLED", "false"),
            ],
        )
        .unwrap();
        assert!(c.notify.email.is_none() && c.notify.healthcheck.is_none());
        assert!(
            from(&[
                ("CUTHULU_NOTIFY_ENABLED", "off"),
                ("CUTHULU_SMTP_TLS", "x"),
                ("CUTHULU_SMTP_HOST", "h")
            ])
            .is_err()
        );
        assert!(from(&[("CUTHULU_NOTIFY_ENABLED", "maybe")]).is_err());
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
