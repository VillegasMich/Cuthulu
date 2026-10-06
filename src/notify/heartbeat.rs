//! Dead-man's switch: ping `CUTHULU_HEALTHCHECK_URL` (e.g. healthchecks.io)
//! every few minutes.
//!
//! A machine that loses power, or a process killed with `SIGKILL`, cannot
//! report anything. The external service notices the missing pings instead
//! and alerts once its grace period runs out. A ping is skipped while a
//! provider is disconnected: Cuthulu that cannot see Docker is not watching
//! anything, so it should be reported as down too.

use std::future::Future;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::HealthcheckConfig;

const PING_TIMEOUT: Duration = Duration::from_secs(10);
/// After a skipped ping (provider disconnected, e.g. right after startup),
/// look again this soon instead of waiting a whole interval.
const SKIP_RETRY: Duration = Duration::from_secs(5);
/// Bytes of the response body read; healthchecks.io answers `OK`.
const BODY_LIMIT: u64 = 4096;

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
            .map_err(|e| format!("healthcheck ping failed: {e}"))?
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
            Err(e) => return Err(format!("healthcheck ping failed: {e}")),
        };
        check_response(&body)
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
/// says so. Logs when pings start failing and when they recover, not on
/// every failed attempt.
pub async fn run<F, Fut>(
    interval: Duration,
    cancel: CancellationToken,
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
            tick.reset_after(SKIP_RETRY.min(interval));
            continue;
        }
        let result = tokio::select! {
            () = cancel.cancelled() => break,
            r = ping() => r,
        };
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

    #[tokio::test]
    async fn bad_url_error_does_not_leak_it() {
        let pinger = Pinger::new(&HealthcheckConfig {
            url: Secret::new("https://exa mple.com/secret-uuid"),
            interval: Duration::from_secs(60),
        });
        let err = pinger.ping().await.unwrap_err();
        assert!(!err.contains("secret-uuid"), "{err}");
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
