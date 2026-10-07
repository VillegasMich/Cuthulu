//! Dead-man's switch: ping `CUTHULU_HEALTHCHECK_URL` (e.g. healthchecks.io)
//! every few minutes.
//!
//! A machine that loses power, or a process killed with `SIGKILL`, cannot
//! report anything. The external service notices the missing pings instead
//! and alerts once its grace period runs out. A ping is skipped while a
//! provider is disconnected: Cuthulu that cannot see Docker is not watching
//! anything, so it should be reported as down too.
//!
//! The latest attempt is kept in a [`PingLog`] for the topbar button; that
//! is all the state there is, and nothing about it is fetched from the
//! healthcheck server.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::lock;
use crate::config::HealthcheckConfig;
use crate::todos::now;

const PING_TIMEOUT: Duration = Duration::from_secs(10);
/// After a skipped ping (provider disconnected, e.g. right after startup),
/// look again this soon instead of waiting a whole interval.
const SKIP_RETRY: Duration = Duration::from_secs(5);
/// Bytes of the response body read; healthchecks.io answers `OK`.
const BODY_LIMIT: u64 = 4096;
/// Longest error text kept for the UI, in characters.
const ERROR_LIMIT: usize = 160;
const REDACTED: &str = "[redacted]";
/// Shortest URL path segment treated as secret on its own: a check UUID is
/// 36 characters, a project ping key 22. Shorter ones (a slug like
/// `backup`) are too common a word to scrub everywhere, and are covered by
/// the whole path being removed.
const SECRET_SEGMENT: usize = 16;

/// How the latest heartbeat went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// Short reason, never containing the ping URL.
    Failed(String),
    /// Not sent: a provider is disconnected.
    Skipped,
}

/// The latest attempt and when it happened (RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastPing {
    pub at: String,
    pub outcome: Outcome,
}

/// Remembers the latest attempt only, so it stays bounded.
#[derive(Debug, Default)]
pub struct PingLog(Mutex<Option<LastPing>>);

impl PingLog {
    pub fn record(&self, outcome: Outcome) {
        *lock(&self.0) = Some(LastPing { at: now(), outcome });
    }

    /// `None` until the first attempt.
    #[must_use]
    pub fn last(&self) -> Option<LastPing> {
        lock(&self.0).clone()
    }
}

/// HTTP GET to the ping URL (blocking `ureq`, run on the blocking pool).
#[derive(Clone)]
pub struct Pinger {
    agent: ureq::Agent,
    url: String,
}

impl Pinger {
    #[must_use]
    pub fn new(config: &HealthcheckConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(PING_TIMEOUT))
            .build()
            .into();
        Self {
            agent,
            url: config.url.expose().to_owned(),
        }
    }

    /// One ping. Errors never contain the URL, which is secret.
    pub async fn ping(&self) -> Result<(), String> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.ping_blocking())
            .await
            .map_err(|e| format!("ping task failed: {e}"))?
            .map_err(|e| redact(&e, &self.url))
    }

    fn ping_blocking(&self) -> Result<(), String> {
        let body = match self.agent.get(&self.url).call() {
            Ok(mut response) => response
                .body_mut()
                .with_config()
                .limit(BODY_LIMIT)
                .read_to_string()
                .unwrap_or_default(),
            // This variant quotes the URL.
            Err(ureq::Error::BadUri(_)) => {
                return Err("CUTHULU_HEALTHCHECK_URL is not a valid URL".to_owned());
            }
            Err(e) => return Err(e.to_string()),
        };
        check_response(&body)
    }
}

/// `error` without the ping URL, its path or any long path segment, cut to
/// [`ERROR_LIMIT`] characters. Error texts from the HTTP client may quote
/// the URL, and it must not reach the logs or the browser.
fn redact(error: &str, url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    let segments = path
        .split(['/', '?', '&', '='])
        .filter(|s| s.len() >= SECRET_SEGMENT);
    let mut out = error.to_owned();
    for secret in [url, rest, path].into_iter().chain(segments) {
        if secret.len() > 1 {
            out = out.replace(secret, REDACTED);
        }
    }
    match out.char_indices().nth(ERROR_LIMIT) {
        Some((i, _)) => format!("{}…", &out[..i]),
        None => out,
    }
}

/// healthchecks.io answers an unknown check with `200 OK (not found)`.
fn check_response(body: &str) -> Result<(), String> {
    if body.contains("not found") {
        return Err(
            "the healthcheck server does not know this ping URL; check CUTHULU_HEALTHCHECK_URL"
                .to_owned(),
        );
    }
    Ok(())
}

/// Pings now and then every `interval` until `cancel` fires, while `alive`
/// says so, recording each attempt in `log`. Logs when pings start failing
/// and when they recover, not on every failed attempt.
pub async fn run<F, Fut>(
    interval: Duration,
    cancel: CancellationToken,
    log: Arc<PingLog>,
    alive: impl Fn() -> bool,
    mut ping: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    info!(
        interval_minutes = interval.as_secs() / 60,
        "sending healthcheck pings"
    );
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut healthy = true;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = tick.tick() => {}
        }
        if !alive() {
            debug!("provider disconnected; healthcheck ping skipped");
            log.record(Outcome::Skipped);
            tick.reset_after(SKIP_RETRY.min(interval));
            continue;
        }
        let result = tokio::select! {
            () = cancel.cancelled() => break,
            r = ping() => r,
        };
        log.record(match &result {
            Ok(()) => Outcome::Sent,
            Err(e) => Outcome::Failed(e.clone()),
        });
        match result {
            Ok(()) if healthy => debug!("healthcheck ping sent"),
            Ok(()) => {
                info!("healthcheck pings work again");
                healthy = true;
            }
            Err(e) if healthy => {
                warn!(error = %e, "healthcheck ping failed; will keep trying");
                healthy = false;
            }
            Err(e) => debug!(error = %e, "healthcheck ping failed"),
        }
    }
    debug!("heartbeat stopped");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::config::Secret;

    #[test]
    fn unknown_check_is_an_error() {
        assert!(check_response("OK").is_ok());
        assert!(check_response("").is_ok());
        assert!(check_response("OK (not found)").is_err());
    }

    #[test]
    fn redact_removes_the_url_and_its_secret_parts() {
        let uuid = "5bf0f1d6-0e1c-4a3c-9a3c-0c6a4c0b0b7e";
        let url = format!("https://hc-ping.com/{uuid}");
        for error in [
            format!("bad request to {url}: 404"),
            format!("GET hc-ping.com/{uuid} failed"),
            format!("path /{uuid} not found"),
            format!("check {uuid}"),
        ] {
            let out = redact(&error, &url);
            assert!(
                !out.contains(uuid) && !out.contains("hc-ping.com/"),
                "{out}"
            );
            assert!(out.contains(REDACTED), "{out}");
        }
        // Ping key + slug: the slug alone is too short to scrub everywhere.
        let url = "https://hc-ping.com/fqOOd6-F4MMNuCEnzTU01w/backup";
        assert_eq!(
            redact("no route to /fqOOd6-F4MMNuCEnzTU01w/backup; backup", url),
            "no route to [redacted]; backup"
        );
        assert_eq!(
            redact("io: Connection refused (os error 111)", url),
            "io: Connection refused (os error 111)"
        );
    }

    #[test]
    fn redact_keeps_errors_short() {
        let out = redact(&"é".repeat(500), "http://x/y");
        assert_eq!(out.chars().count(), ERROR_LIMIT + 1);
        assert!(out.ends_with('…'));
    }

    #[tokio::test]
    async fn unreachable_url_error_does_not_leak_it() {
        let pinger = Pinger::new(&HealthcheckConfig {
            url: Secret::new("http://127.0.0.1:9/secret-uuid-0123456789"),
            interval: Duration::from_secs(60),
        });
        let err = pinger.ping().await.unwrap_err();
        assert!(
            !err.contains("secret-uuid") && !err.contains("127.0.0.1:9"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn bad_url_error_does_not_leak_it() {
        let pinger = Pinger::new(&HealthcheckConfig {
            url: Secret::new("https://exa mple.com/secret-uuid"),
            interval: Duration::from_secs(60),
        });
        let err = pinger.ping().await.unwrap_err();
        assert!(!err.contains("secret-uuid"), "{err}");
    }

    /// Waits (up to 10 s) until `log` holds `want`.
    async fn wait_for(log: &PingLog, want: &Outcome) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while log.last().map(|l| l.outcome).as_ref() != Some(want) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {want:?}, got {:?}", log.last()));
    }

    #[tokio::test]
    async fn records_the_latest_attempt() {
        let cancel = CancellationToken::new();
        let log = Arc::new(PingLog::default());
        assert_eq!(log.last(), None, "nothing before the first attempt");
        let alive = Arc::new(AtomicBool::new(true));
        let fail = Arc::new(AtomicBool::new(false));
        let task = {
            let (cancel, log, alive, fail) =
                (cancel.clone(), log.clone(), alive.clone(), fail.clone());
            tokio::spawn(run(
                Duration::from_millis(10),
                cancel,
                log,
                move || alive.load(Ordering::SeqCst),
                move || {
                    let fail = fail.load(Ordering::SeqCst);
                    async move {
                        if fail {
                            Err("io: refused".to_owned())
                        } else {
                            Ok(())
                        }
                    }
                },
            ))
        };
        wait_for(&log, &Outcome::Sent).await;
        fail.store(true, Ordering::SeqCst);
        wait_for(&log, &Outcome::Failed("io: refused".into())).await;
        alive.store(false, Ordering::SeqCst);
        wait_for(&log, &Outcome::Skipped).await;
        assert!(log.last().unwrap().at.ends_with('Z'));

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn pings_every_interval_until_cancelled_and_skips_while_dead() {
        let cancel = CancellationToken::new();
        let pings = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let task = {
            let (cancel, pings, alive) = (cancel.clone(), pings.clone(), alive.clone());
            tokio::spawn(run(
                Duration::from_millis(20),
                cancel,
                Arc::default(),
                move || alive.load(Ordering::SeqCst),
                move || {
                    let n = pings.fetch_add(1, Ordering::SeqCst);
                    // Failures do not stop the loop.
                    async move {
                        if n == 1 {
                            Err("down".to_owned())
                        } else {
                            Ok(())
                        }
                    }
                },
            ))
        };
        tokio::time::sleep(Duration::from_millis(110)).await;
        let sent = pings.load(Ordering::SeqCst);
        assert!(sent >= 3, "{sent} pings");

        alive.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let paused = pings.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(pings.load(Ordering::SeqCst), paused, "no pings while dead");

        alive.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(pings.load(Ordering::SeqCst) > paused, "pings resume");

        cancel.cancel();
        task.await.unwrap();
    }
}
