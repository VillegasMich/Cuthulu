//! Notifications: service-down / back-up emails for watched services, a
//! shutdown email, and a healthcheck heartbeat.
//!
//! Driven entirely by the registry's broadcast (no polling): a
//! [`alerts::Tracker`] turns changes into alerts, which go through a
//! bounded outbox to the [`mail::Mailer`]. Which services are watched, and
//! the global switch, live in [`store::NotifyStore`]. Browser notifications
//! are done client-side from the same `/api/events` stream.

pub mod alerts;
pub mod heartbeat;
pub mod mail;
pub mod store;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use self::alerts::Tracker;
use self::heartbeat::Pinger;
use self::mail::{Mailer, Message, SmtpMailer};
pub use self::store::{NotifyStore, Settings, StoreError};
use crate::config::Config;
use crate::registry::{Registry, RegistryEvent};
use crate::todos::now;

/// How long a change must last before it is mailed (see [`alerts`]).
pub const SETTLE: Duration = Duration::from_secs(30);
/// Emails waiting to be sent; more are dropped (and logged).
const OUTBOX: usize = 32;
const SEND_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(10);
/// The shutdown email must fit in `docker stop`'s default 10 s grace.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(8);

/// What the UI needs to know. Never carries addresses, URLs or secrets.
#[derive(Debug, Clone, Serialize)]
pub struct NotifyState {
    /// Global switch for service-down alerts.
    pub enabled: bool,
    /// Watched service names, sorted.
    pub watched: Vec<String>,
    /// Email is configured (and `CUTHULU_NOTIFY_ENABLED` is not false).
    pub email: bool,
    /// A healthcheck URL is configured.
    pub healthcheck: bool,
    pub cooldown_minutes: u64,
    /// Running services that were stopped through Cuthulu and then started
    /// by something else (a systemd unit, a restart policy), sorted.
    pub restarted_elsewhere: Vec<String>,
}

/// Outcome of `POST /api/notify/test`, per channel.
#[derive(Debug, Serialize)]
pub struct TestReport {
    pub email: ChannelResult,
    pub healthcheck: ChannelResult,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ChannelResult {
    /// Not configured.
    Off,
    Sent,
    Failed {
        error: String,
    },
}

pub struct Notifier {
    store: NotifyStore,
    mailer: Option<Arc<dyn Mailer>>,
    pinger: Option<Pinger>,
    heartbeat_interval: Duration,
    cooldown: Duration,
    host: String,
    started: Instant,
    started_at: String,
    tracker: Mutex<Tracker>,
    outbox: mpsc::Sender<Message>,
    outbox_rx: Mutex<Option<mpsc::Receiver<Message>>>,
}

impl Notifier {
    #[must_use]
    pub fn new(config: &Config) -> Arc<Self> {
        let mailer = config.notify.email.as_ref().and_then(|e| {
            SmtpMailer::new(e)
                .inspect_err(|err| error!(error = %err, "email notifications disabled"))
                .ok()
                .map(|m| Arc::new(m) as Arc<dyn Mailer>)
        });
        Self::with_parts(config, mailer, SETTLE)
    }

    fn with_parts(config: &Config, mailer: Option<Arc<dyn Mailer>>, settle: Duration) -> Arc<Self> {
        let n = &config.notify;
        let (outbox, rx) = mpsc::channel(OUTBOX);
        let notifier = Self {
            store: NotifyStore::open(&config.data_dir),
            mailer,
            pinger: n.healthcheck.as_ref().map(Pinger::new),
            heartbeat_interval: n
                .healthcheck
                .as_ref()
                .map_or(Duration::from_secs(300), |h| h.interval),
            cooldown: n.cooldown,
            host: host_name(n.host.as_deref()),
            started: Instant::now(),
            started_at: now(),
            tracker: Mutex::new(Tracker::new(settle, n.cooldown)),
            outbox,
            outbox_rx: Mutex::new(Some(rx)),
        };
        info!(
            email = notifier.mailer.is_some(),
            healthcheck = notifier.pinger.is_some(),
            host = %notifier.host,
            "notifications"
        );
        Arc::new(notifier)
    }

    /// Starts the alert loop, the email sender and the heartbeat. Call
    /// before [`Registry::spawn`] so the first listing is seen as such.
    pub fn spawn(
        self: &Arc<Self>,
        registry: &Arc<Registry>,
        cancel: &CancellationToken,
    ) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        let rx = registry.subscribe();
        let (this, reg, c) = (Arc::clone(self), Arc::clone(registry), cancel.clone());
        tasks.push(tokio::spawn(async move { this.watch(reg, rx, c).await }));

        if let (Some(mailer), Some(rx)) = (&self.mailer, lock(&self.outbox_rx).take()) {
            let (mailer, c) = (Arc::clone(mailer), cancel.clone());
            tasks.push(tokio::spawn(send_loop(mailer, rx, c)));
        }

        if let Some(pinger) = self.pinger.clone() {
            let reg = Arc::clone(registry);
            tasks.push(tokio::spawn(heartbeat::run(
                self.heartbeat_interval,
                cancel.clone(),
                move || reg.statuses().iter().all(|s| s.connected),
                move || {
                    let p = pinger.clone();
                    async move { p.ping().await }
                },
            )));
        }
        tasks
    }

    pub fn state(&self) -> Result<NotifyState, StoreError> {
        Ok(self.state_of(self.store.get()?))
    }

    pub async fn set_enabled(&self, on: bool) -> Result<NotifyState, StoreError> {
        let s = self.store.set_enabled(on).await?;
        info!(enabled = on, "service alerts switched");
        Ok(self.state_of(s))
    }

    pub async fn set_watched(&self, name: &str, on: bool) -> Result<NotifyState, StoreError> {
        let s = self.store.set_watched(name, on).await?;
        info!(service = name, watched = on, "watch list changed");
        Ok(self.state_of(s))
    }

    /// The operator asked Cuthulu to act on `name`; the down flip that
    /// follows is not an outage.
    pub fn expect(&self, name: &str, stop: bool) {
        lock(&self.tracker).expect(name, stop, Instant::now());
    }

    /// Sends a test email and one healthcheck ping, right now.
    pub async fn test(&self) -> TestReport {
        let email = match &self.mailer {
            None => ChannelResult::Off,
            Some(m) => match m.send(&mail::test_message(&self.host, &now())).await {
                Ok(()) => ChannelResult::Sent,
                Err(e) => ChannelResult::Failed { error: e.message },
            },
        };
        let healthcheck = match &self.pinger {
            None => ChannelResult::Off,
            Some(p) => match p.ping().await {
                Ok(()) => ChannelResult::Sent,
                Err(error) => ChannelResult::Failed { error },
            },
        };
        info!(?email, ?healthcheck, "test notification");
        TestReport { email, healthcheck }
    }

    /// Reports a clean shutdown by email, giving up after 8 s.
    pub async fn stopped(&self) {
        let Some(mailer) = &self.mailer else {
            return;
        };
        let watched = self.store.get().map(|s| s.watched).unwrap_or_default();
        let down = lock(&self.tracker).down(|n| watched.contains(n));
        let message = mail::stop_message(
            &self.host,
            &self.started_at,
            &now(),
            self.started.elapsed(),
            &down,
            self.pinger.is_some(),
        );
        match tokio::time::timeout(SHUTDOWN_BUDGET, mailer.send(&message)).await {
            Ok(Ok(())) => info!(subject = %message.subject, "notification sent"),
            Ok(Err(e)) => warn!(error = %e, "could not send the shutdown email"),
            Err(_) => warn!("shutdown email timed out"),
        }
    }

    fn state_of(&self, s: Settings) -> NotifyState {
        NotifyState {
            enabled: s.enabled,
            watched: s.watched.into_iter().collect(),
            email: self.mailer.is_some(),
            healthcheck: self.pinger.is_some(),
            cooldown_minutes: self.cooldown.as_secs() / 60,
            restarted_elsewhere: lock(&self.tracker).started_elsewhere(),
        }
    }

    async fn watch(
        self: Arc<Self>,
        registry: Arc<Registry>,
        mut rx: broadcast::Receiver<RegistryEvent>,
        cancel: CancellationToken,
    ) {
        self.resync(&registry);
        loop {
            let next = lock(&self.tracker).next_due();
            let wake = async {
                match next {
                    Some(t) => tokio::time::sleep_until(t.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = cancel.cancelled() => break,
                ev = rx.recv() => match ev {
                    Ok(RegistryEvent::Upsert(s)) => lock(&self.tracker).upsert(&s, Instant::now()),
                    Ok(RegistryEvent::Remove(id)) => {
                        let watched = self.store.get().map(|s| s.watched).unwrap_or_default();
                        lock(&self.tracker).remove(&id, Instant::now(), |n| watched.contains(n));
                    }
                    Ok(RegistryEvent::Status(_)) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => self.resync(&registry),
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                () = wake => {}
            }
            self.flush();
        }
        debug!("alert loop stopped");
    }

    /// Brings the tracker in line with the registry after missed events.
    fn resync(&self, registry: &Registry) {
        let services = registry.snapshot();
        let watched = self.store.get().map(|s| s.watched).unwrap_or_default();
        let now = Instant::now();
        let mut tracker = lock(&self.tracker);
        let present: HashSet<_> = services.iter().map(|s| &s.id).collect();
        for id in tracker.ids() {
            if !present.contains(&id) {
                tracker.remove(&id, now, |n| watched.contains(n));
            }
        }
        for s in &services {
            tracker.upsert(s, now);
        }
    }

    /// Queues the emails for every alert that is due.
    fn flush(&self) {
        let settings = self.store.get().ok();
        let alerts = lock(&self.tracker).due(Instant::now(), |n| {
            settings.as_ref().is_some_and(|s| s.alerts_for(n))
        });
        for alert in alerts {
            info!(
                service = %alert.name,
                kind = ?alert.kind,
                detail = %alert.detail,
                "service alert"
            );
            if self.mailer.is_none() {
                continue;
            }
            let message = mail::alert_message(&self.host, &alert, &now(), self.cooldown);
            if self.outbox.try_send(message).is_err() {
                warn!(service = %alert.name, "email outbox full; alert dropped");
            }
        }
    }
}

/// Delivers queued emails one by one, retrying transient failures.
async fn send_loop(
    mailer: Arc<dyn Mailer>,
    mut rx: mpsc::Receiver<Message>,
    cancel: CancellationToken,
) {
    loop {
        let message = tokio::select! {
            () = cancel.cancelled() => break,
            m = rx.recv() => match m {
                Some(m) => m,
                None => break,
            },
        };
        tokio::select! {
            () = cancel.cancelled() => break,
            () = deliver(mailer.as_ref(), &message) => {}
        }
    }
}

async fn deliver(mailer: &dyn Mailer, message: &Message) {
    for attempt in 1..=SEND_ATTEMPTS {
        match mailer.send(message).await {
            Ok(()) => {
                info!(subject = %message.subject, "notification sent");
                return;
            }
            Err(e) if e.permanent || attempt == SEND_ATTEMPTS => {
                warn!(error = %e, subject = %message.subject, "could not send notification");
                return;
            }
            Err(e) => {
                debug!(error = %e, attempt, "sending notification failed; retrying");
                tokio::time::sleep(RETRY_DELAY * attempt).await;
            }
        }
    }
}

/// `configured`, else the host name when not in a container (inside one
/// the kernel host name names the container), else `cuthulu`.
fn host_name(configured: Option<&str>) -> String {
    configured
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            if Path::new("/.dockerenv").exists() {
                return None;
            }
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "cuthulu".to_owned())
}

/// Plain data, valid after any panic: poisoning is ignored.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::config::{EmailConfig, HealthcheckConfig, NotifyConfig, Secret, SmtpTls};
    use crate::model::ServiceState;
    use crate::providers::ProviderEvent;
    use crate::registry::tests::{MockProvider, service};
    use crate::todos::tests::TempDir;

    /// What a fake server received, shared with its thread.
    type Inbox = Arc<Mutex<Vec<String>>>;

    /// A server on a free localhost port handling each connection with
    /// `session` in a background thread.
    fn fake_server(session: fn(TcpStream, &Inbox) -> std::io::Result<()>) -> (u16, Inbox) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let inbox = Inbox::default();
        let recorded = Arc::clone(&inbox);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = session(stream, &recorded);
            }
        });
        (port, inbox)
    }

    /// Minimal SMTP server: accepts every message and records it.
    fn smtp_session(stream: TcpStream, inbox: &Inbox) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut out = stream;
        out.write_all(b"220 localhost fake SMTP\r\n")?;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            match line.trim_end().to_ascii_uppercase().as_str() {
                "DATA" => {
                    out.write_all(b"354 go ahead\r\n")?;
                    let mut data = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line)? == 0 || line.trim_end() == "." {
                            break;
                        }
                        data.push_str(&line);
                    }
                    // Undo header folding (RFC 5322).
                    let message = data.replace("\r\n ", " ").replace("\r\n\t", " ");
                    inbox.lock().unwrap().push(message);
                    out.write_all(b"250 queued\r\n")?;
                }
                "QUIT" => return out.write_all(b"221 bye\r\n"),
                _ => out.write_all(b"250 ok\r\n")?,
            }
        }
    }

    /// Minimal HTTP server: records the request line, answers like healthchecks.io.
    fn http_session(stream: TcpStream, inbox: &Inbox) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request = String::new();
        reader.read_line(&mut request)?;
        let mut header = String::new();
        while reader.read_line(&mut header)? > 2 {
            header.clear();
        }
        inbox.lock().unwrap().push(request.trim_end().to_owned());
        let mut out = stream;
        out.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
    }

    fn subjects(inbox: &Inbox) -> Vec<String> {
        inbox
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                m.lines()
                    .find_map(|l| l.strip_prefix("Subject: "))
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    async fn wait_for(inbox: &Inbox, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while inbox.lock().unwrap().len() < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {n} messages, got {:?}", inbox.lock().unwrap()));
    }

    /// Config with email and pings going to fake servers on localhost.
    pub(crate) fn local_config(dir: &Path, smtp_port: u16, http_port: u16) -> Config {
        Config {
            data_dir: dir.to_owned(),
            notify: NotifyConfig {
                email: Some(EmailConfig {
                    smtp_host: "127.0.0.1".into(),
                    smtp_port,
                    tls: SmtpTls::None,
                    credentials: None,
                    from: "cuthulu <cuthulu@example.com>".parse().unwrap(),
                    to: "ops@example.com".parse().unwrap(),
                }),
                healthcheck: Some(HealthcheckConfig {
                    url: Secret::new(format!("http://127.0.0.1:{http_port}/ping/abc")),
                    interval: Duration::from_secs(3600),
                }),
                host: Some("box".into()),
                ..NotifyConfig::default()
            },
            ..Config::default()
        }
    }

    /// End to end against a fake SMTP and a fake ping server: heartbeat,
    /// down and back-up emails driven by provider events, the test
    /// notification and the shutdown email.
    #[tokio::test]
    async fn emails_and_pings_against_fake_servers() {
        let (smtp_port, emails) = fake_server(smtp_session);
        let (http_port, pings) = fake_server(http_session);
        let dir = TempDir::new();
        let config = local_config(dir.path(), smtp_port, http_port);

        let (tx, rx) = unbounded_channel();
        let web = service("web", ServiceState::Running);
        let provider = Arc::new(MockProvider::with(vec![
            web.clone(),
            service("db", ServiceState::Running),
        ]));
        *provider.events.lock().unwrap() = Some(rx);
        let registry = Registry::new(vec![provider.clone()]);

        let mailer = SmtpMailer::new(config.notify.email.as_ref().unwrap()).unwrap();
        let notifier =
            Notifier::with_parts(&config, Some(Arc::new(mailer)), Duration::from_millis(50));
        notifier.set_watched("web", true).await.unwrap();
        let cancel = CancellationToken::new();
        let mut tasks = notifier.spawn(&registry, &cancel);
        tasks.extend(registry.spawn(Duration::from_secs(3600), &cancel));

        let state = notifier.state().unwrap();
        assert_eq!(state.watched, ["web"]);
        assert!(state.email && state.healthcheck);

        // `web` crashes: one email after settling. `db` is not watched.
        let set_state = |name: &str, state: ServiceState, exit: Option<i64>| {
            let mut services = provider.services.lock().unwrap();
            let s = services.iter_mut().find(|s| s.name == name).unwrap();
            s.state = state;
            s.exit_code = exit;
            s.id.clone()
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let id = set_state("web", ServiceState::Stopped, Some(1));
        tx.send(Ok(ProviderEvent::Changed(id))).unwrap();
        let id = set_state("db", ServiceState::Stopped, Some(1));
        tx.send(Ok(ProviderEvent::Changed(id))).unwrap();
        wait_for(&emails, 1).await;

        let id = set_state("web", ServiceState::Running, None);
        tx.send(Ok(ProviderEvent::Changed(id))).unwrap();
        wait_for(&emails, 2).await;

        let report = notifier.test().await;
        assert!(matches!(report.email, ChannelResult::Sent), "{report:?}");
        assert!(
            matches!(report.healthcheck, ChannelResult::Sent),
            "{report:?}"
        );
        wait_for(&emails, 3).await;

        // A stop requested through Cuthulu is reported as such, even
        // within the cooldown of the crash above.
        notifier.expect("web", true);
        let id = set_state("web", ServiceState::Stopped, Some(0));
        tx.send(Ok(ProviderEvent::Changed(id))).unwrap();
        wait_for(&emails, 4).await;

        cancel.cancel();
        for t in tasks {
            t.await.unwrap();
        }
        notifier.stopped().await;
        wait_for(&emails, 5).await;

        assert_eq!(
            subjects(&emails),
            [
                "[cuthulu] box: web is down (exited 1)",
                "[cuthulu] box: web is back up",
                "[cuthulu] box: test notification",
                "[cuthulu] box: web stopped from cuthulu (exited 0)",
                "[cuthulu] box: cuthulu stopped",
            ]
        );
        let emails = emails.lock().unwrap().clone();
        assert!(
            emails[0].contains("From: cuthulu <cuthulu@example.com>"),
            "{}",
            emails[0]
        );
        assert!(emails[0].contains("To: ops@example.com"), "{}", emails[0]);
        assert!(
            emails[4].contains("Watched services down: web"),
            "{}",
            emails[4]
        );

        let pings = pings.lock().unwrap();
        assert!(
            !pings.is_empty() && pings.iter().all(|p| p == "GET /ping/abc HTTP/1.1"),
            "{pings:?}"
        );
    }

    #[tokio::test]
    async fn unreachable_smtp_is_reported_by_the_test() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let dir = TempDir::new();
        let mut config = local_config(dir.path(), port, port);
        config.notify.healthcheck = None;
        let notifier = Notifier::new(&config);
        let report = notifier.test().await;
        assert!(
            matches!(report.email, ChannelResult::Failed { .. }),
            "{report:?}"
        );
        assert!(matches!(report.healthcheck, ChannelResult::Off));
    }

    #[test]
    fn host_name_prefers_the_setting() {
        assert_eq!(host_name(Some("home")), "home");
        assert!(!host_name(None).is_empty());
    }
}
