//! Email: the [`Mailer`] seam, its SMTP implementation, and the texts.
//!
//! Every subject starts with [`SUBJECT_PREFIX`] and the host, so a mail
//! filter can label them. Plain text only.

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use super::alerts::{Alert, AlertKind};
use crate::config::{EmailConfig, SmtpTls};

pub const SUBJECT_PREFIX: &str = "[cuthulu]";

const SMTP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub subject: String,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct MailError {
    /// Bad credentials or a rejected address: retrying will not help.
    pub permanent: bool,
    pub message: String,
}

/// Delivers a message. [`SmtpMailer`] in production; faked in tests.
#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, message: &Message) -> Result<(), MailError>;
}

/// Plain-text email over SMTP, encrypted as configured.
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    to: Mailbox,
}

impl SmtpMailer {
    pub fn new(config: &EmailConfig) -> Result<Self, MailError> {
        type Smtp = AsyncSmtpTransport<Tokio1Executor>;
        let builder = match config.tls {
            SmtpTls::Implicit => Smtp::relay(&config.smtp_host),
            SmtpTls::StartTls => Smtp::starttls_relay(&config.smtp_host),
            SmtpTls::None => Ok(Smtp::builder_dangerous(&config.smtp_host)),
        }
        .map_err(|e| MailError {
            permanent: true,
            message: format!("invalid CUTHULU_SMTP_HOST {:?}: {e}", config.smtp_host),
        })?
        .port(config.smtp_port)
        .timeout(Some(SMTP_TIMEOUT));
        let builder = match &config.credentials {
            Some(c) => builder.credentials(Credentials::new(
                c.username.clone(),
                c.password.expose().to_owned(),
            )),
            None => builder,
        };
        Ok(Self {
            transport: builder.build(),
            from: config.from.clone(),
            to: config.to.clone(),
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let email = lettre::Message::builder()
            .from(self.from.clone())
            .to(self.to.clone())
            .subject(&message.subject)
            .header(ContentType::TEXT_PLAIN)
            .body(message.body.clone())
            .map_err(|e| MailError {
                permanent: true,
                message: format!("cannot build email: {e}"),
            })?;
        self.transport
            .send(email)
            .await
            .map(drop)
            .map_err(|e| MailError {
                permanent: e.is_permanent(),
                message: format!("SMTP: {e}"),
            })
    }
}

fn subject(host: &str, what: &str) -> String {
    format!("{SUBJECT_PREFIX} {host}: {what}")
}

/// The email for a down / back-up alert.
#[must_use]
pub fn alert_message(host: &str, alert: &Alert, now: &str, cooldown: Duration) -> Message {
    let name = &alert.name;
    let (what, mut body) = match alert.kind {
        AlertKind::Down if alert.by_operator => (
            format!("{name} stopped from cuthulu ({})", alert.detail),
            format!(
                "Service {name} on {host} was stopped from the cuthulu dashboard: {detail}.\n\n\
                 Noticed at: {now}\n",
                detail = alert.detail
            ),
        ),
        AlertKind::Down => (
            format!("{name} is down ({})", alert.detail),
            format!(
                "Service {name} on {host} is down: {detail}.\n\n\
                 Noticed at: {now}\n",
                detail = alert.detail
            ),
        ),
        AlertKind::Restarted => (
            format!("{name} crashed and was restarted ({})", alert.detail),
            format!(
                "Service {name} on {host} crashed ({detail}) and was running again \
                 {down} later, started by a restart policy or a systemd unit.\n\n\
                 Back at:    {now}\n",
                detail = alert.detail,
                down = format_duration(alert.down_for.unwrap_or_default()),
            ),
        ),
        AlertKind::Up => (
            format!("{name} is back up"),
            format!(
                "Service {name} on {host} is {detail} again.\n\n\
                 Back at:    {now}\n\
                 Down for:   {down}\n",
                detail = alert.detail,
                down = format_duration(alert.down_for.unwrap_or_default()),
            ),
        ),
    };
    if alert.started_elsewhere {
        body.push_str(
            "\nIt was stopped from cuthulu but started again by something else: a systemd \
             unit (Restart=), a restart policy or a person. To keep it stopped, stop it there, \
             e.g. `systemctl stop <unit>`.\n",
        );
    }
    if alert.flaps > 0 {
        let _ = writeln!(
            body,
            "It also went down {} since the last email.",
            times(alert.flaps)
        );
    }
    if alert.kind == AlertKind::Down && alert.by_operator {
        body.push_str("\nYou get one email when it is back up.\n");
    } else if alert.kind == AlertKind::Down {
        let _ = write!(
            body,
            "\nYou get one email when it is back up. Further outages of {name} within {} \
             of this email are summed up in a later one.\n",
            format_duration(cooldown)
        );
    } else if alert.kind == AlertKind::Restarted {
        let _ = write!(
            body,
            "\nFurther crashes of {name} within {} of this email are summed up in a later \
             one. Check its logs in the dashboard.\n",
            format_duration(cooldown)
        );
    }
    body.push_str(FOOTER);
    Message {
        subject: subject(host, &what),
        body,
    }
}

/// The email sent when Cuthulu itself shuts down cleanly.
#[must_use]
pub fn stop_message(
    host: &str,
    started_at: &str,
    now: &str,
    uptime: Duration,
    down: &[String],
    heartbeat: bool,
) -> Message {
    let mut body = format!(
        "Cuthulu on {host} was asked to stop (docker stop, compose down/up, reboot or \
         Ctrl-C) and exited cleanly. Until it is back, nobody is watching the services.\n\n\
         Stopped at:    {now}\n\
         Running since: {started_at} ({})\n",
        format_duration(uptime)
    );
    if !down.is_empty() {
        let _ = writeln!(body, "Watched services down: {}", down.join(", "));
    }
    if heartbeat {
        body.push_str(
            "\nIf it does not come back, the healthcheck reports it as down once its grace \
             period runs out.\n",
        );
    }
    body.push_str(FOOTER);
    Message {
        subject: subject(host, "cuthulu stopped"),
        body,
    }
}

#[must_use]
pub fn test_message(host: &str, now: &str) -> Message {
    Message {
        subject: subject(host, "test notification"),
        body: format!(
            "This is a test from Cuthulu on {host}, sent at {now}. Email notifications work.\n\
             {FOOTER}"
        ),
    }
}

const FOOTER: &str = "\n-- \nSent by cuthulu. Choose watched services with the bell in the \
                      dashboard; turn alerts off there or with CUTHULU_NOTIFY_ENABLED=false.\n";

fn times(n: u32) -> String {
    if n == 1 {
        "once more".to_owned()
    } else {
        format!("{n} more times")
    }
}

/// `2d 3h 4m`, `5h 0m`, `12m`, `40s`.
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) => format!("{hours}h {mins}m"),
        _ => format!("{days}d {hours}h {mins}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(kind: AlertKind, detail: &str, flaps: u32) -> Alert {
        Alert {
            kind,
            name: "web".into(),
            detail: detail.into(),
            flaps,
            down_for: (kind != AlertKind::Down).then_some(Duration::from_secs(3720)),
            by_operator: false,
            started_elsewhere: false,
        }
    }

    const NOW: &str = "2026-10-05T12:00:00Z";
    const COOLDOWN: Duration = Duration::from_secs(900);

    #[test]
    fn down_and_up_subjects_and_bodies() {
        let m = alert_message("box", &alert(AlertKind::Down, "exited 1", 0), NOW, COOLDOWN);
        assert_eq!(m.subject, "[cuthulu] box: web is down (exited 1)");
        assert!(m.body.starts_with("Service web on box is down: exited 1."));
        assert!(m.body.contains("Noticed at: 2026-10-05T12:00:00Z"));
        assert!(m.body.contains("within 15m"));
        assert!(!m.body.contains("since the last email"));

        let m = alert_message("box", &alert(AlertKind::Down, "removed", 3), NOW, COOLDOWN);
        assert!(
            m.body
                .contains("went down 3 more times since the last email")
        );

        let m = alert_message("box", &alert(AlertKind::Up, "running", 1), NOW, COOLDOWN);
        assert_eq!(m.subject, "[cuthulu] box: web is back up");
        assert!(m.body.contains("Down for:   1h 2m"));
        assert!(m.body.contains("went down once more"));
    }

    #[test]
    fn operator_restart_and_started_elsewhere_texts() {
        let mut a = alert(AlertKind::Down, "exited 143", 0);
        a.by_operator = true;
        let m = alert_message("box", &a, NOW, COOLDOWN);
        assert_eq!(
            m.subject,
            "[cuthulu] box: web stopped from cuthulu (exited 143)"
        );
        assert!(m.body.contains("stopped from the cuthulu dashboard"));
        assert!(!m.body.contains("summed up"), "no cooldown for own stops");

        let mut a = alert(AlertKind::Up, "running", 0);
        a.started_elsewhere = true;
        let m = alert_message("box", &a, NOW, COOLDOWN);
        assert_eq!(m.subject, "[cuthulu] box: web is back up");
        assert!(m.body.contains("systemctl stop"), "{}", m.body);

        let m = alert_message(
            "box",
            &alert(AlertKind::Restarted, "exited 3", 2),
            NOW,
            COOLDOWN,
        );
        assert_eq!(
            m.subject,
            "[cuthulu] box: web crashed and was restarted (exited 3)"
        );
        assert!(m.body.contains("running again 1h 2m later"), "{}", m.body);
        assert!(m.body.contains("went down 2 more times"));
    }

    #[test]
    fn stop_message_summarizes() {
        let m = stop_message(
            "box",
            "2026-10-03T09:30:00Z",
            NOW,
            Duration::from_secs(2 * 86_400 + 2 * 3600 + 30 * 60),
            &["db".into()],
            true,
        );
        assert_eq!(m.subject, "[cuthulu] box: cuthulu stopped");
        assert!(
            m.body
                .contains("Running since: 2026-10-03T09:30:00Z (2d 2h 30m)")
        );
        assert!(m.body.contains("Watched services down: db"));
        assert!(m.body.contains("healthcheck"));
        let m = stop_message("box", NOW, NOW, Duration::ZERO, &[], false);
        assert!(!m.body.contains("healthcheck") && !m.body.contains("Watched"));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_duration(Duration::from_secs(61 * 60)), "1h 1m");
        assert_eq!(
            format_duration(Duration::from_secs((1440 * 3 + 5) * 60)),
            "3d 0h 5m"
        );
    }
}
