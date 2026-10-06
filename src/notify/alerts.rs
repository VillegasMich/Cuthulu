//! Turns registry changes into "down" / "back up" alerts. Pure: time is
//! passed in, nothing is sent from here.
//!
//! Rules, per service *name* (a re-created container keeps its alerts):
//!
//! - **Settle:** a change must last [`Tracker::new`]'s `settle` before it is
//!   reported, so a `docker restart` or `docker compose up` re-creating the
//!   container stays silent.
//! - **Cooldown:** at most one "down" alert per service per `cooldown`. A
//!   crash loop gives one alert, then (if it is still down) one more when the
//!   cooldown ends, mentioning how often it went down in between.
//! - **One per transition:** "back up" is sent only after a "down" was.
//! - **Operator actions:** a stop/restart requested through Cuthulu silences
//!   the down flip that follows within [`QUIET_WINDOW`] — the operator is
//!   looking at the dashboard and knows.
//! - Services first seen while down, and services not watched when they
//!   went down, are never reported for that outage.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::model::{Health, Service, ServiceId, ServiceState};

/// How long after a stop/restart requested through Cuthulu a down flip is
/// treated as intended.
pub const QUIET_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Down,
    Up,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: AlertKind,
    pub name: String,
    /// State as text: `exited 1`, `unhealthy`, `removed`, `running`, …
    pub detail: String,
    /// Down flips since the last alert that are not reported otherwise.
    pub flaps: u32,
    /// For [`AlertKind::Up`]: how long the outage lasted.
    pub down_for: Option<Duration>,
}

#[derive(Debug)]
struct Entry {
    up: bool,
    detail: String,
    /// When `up` last flipped.
    changed_at: Instant,
    /// Start of the outage a "down" alert was sent for.
    announced_down: Option<Instant>,
    last_down_alert: Option<Instant>,
    flaps: u32,
    /// The current outage is not to be reported.
    silenced: bool,
    quiet_until: Option<Instant>,
}

#[derive(Debug)]
pub struct Tracker {
    settle: Duration,
    cooldown: Duration,
    entries: HashMap<String, Entry>,
    /// Name of every service id currently known, for `Remove` events.
    names: HashMap<ServiceId, String>,
}

/// `(up, detail)` for a service, or `None` when it says nothing about
/// health: Cuthulu itself, or a state the provider could not read.
#[must_use]
pub fn classify(s: &Service) -> Option<(bool, String)> {
    if s.is_self {
        return None;
    }
    let unhealthy = s.health == Health::Unhealthy;
    let up = matches!(s.state, ServiceState::Running | ServiceState::Paused) && !unhealthy;
    let detail = match s.state {
        ServiceState::Unknown => return None,
        _ if unhealthy => "unhealthy".to_owned(),
        ServiceState::Running => "running".to_owned(),
        ServiceState::Paused => "paused".to_owned(),
        ServiceState::Restarting => "restarting".to_owned(),
        ServiceState::Created => "created".to_owned(),
        ServiceState::Dead => "dead".to_owned(),
        ServiceState::Stopped => s
            .exit_code
            .map_or_else(|| "stopped".to_owned(), |c| format!("exited {c}")),
    };
    Some((up, detail))
}

impl Tracker {
    #[must_use]
    pub fn new(settle: Duration, cooldown: Duration) -> Self {
        Self {
            settle,
            cooldown,
            entries: HashMap::new(),
            names: HashMap::new(),
        }
    }

    pub fn upsert(&mut self, s: &Service, now: Instant) {
        let Some((up, detail)) = classify(s) else {
            return;
        };
        self.names.insert(s.id.clone(), s.name.clone());
        self.observe(&s.name, up, detail, now);
    }

    /// A service disappeared. Watched names count it as down ("removed");
    /// unwatched ones are forgotten, which keeps the map bounded.
    pub fn remove(&mut self, id: &ServiceId, now: Instant, watched: impl Fn(&str) -> bool) {
        let Some(name) = self.names.remove(id) else {
            return;
        };
        if self.names.values().any(|n| *n == name) {
            return;
        }
        if watched(&name) {
            self.observe(&name, false, "removed".to_owned(), now);
        } else {
            self.entries.remove(&name);
        }
    }

    /// The operator asked Cuthulu to stop/restart `name`.
    pub fn expect(&mut self, name: &str, now: Instant) {
        if let Some(e) = self.entries.get_mut(name) {
            e.quiet_until = Some(now + QUIET_WINDOW);
        }
    }

    fn observe(&mut self, name: &str, up: bool, detail: String, now: Instant) {
        let Some(e) = self.entries.get_mut(name) else {
            self.entries.insert(
                name.to_owned(),
                Entry {
                    up,
                    detail,
                    changed_at: now,
                    announced_down: None,
                    last_down_alert: None,
                    flaps: 0,
                    // Never report an outage that began before we looked.
                    silenced: !up,
                    quiet_until: None,
                },
            );
            return;
        };
        e.detail = detail;
        if e.up == up {
            return;
        }
        e.up = up;
        e.changed_at = now;
        if !up {
            e.flaps += 1;
            e.silenced = e.quiet_until.is_some_and(|t| now < t);
        }
    }

    /// Alerts that are due at `now`. `alerts_for` says whether a name is
    /// watched (and alerts are switched on).
    pub fn due(&mut self, now: Instant, alerts_for: impl Fn(&str) -> bool) -> Vec<Alert> {
        let mut out = Vec::new();
        for (name, e) in &mut self.entries {
            if now < e.changed_at + self.settle {
                continue;
            }
            let cooling = e.last_down_alert.is_some_and(|t| now < t + self.cooldown);
            if !e.up && e.announced_down.is_none() && !e.silenced {
                if !alerts_for(name) {
                    e.silenced = true;
                } else if !cooling {
                    out.push(Alert {
                        kind: AlertKind::Down,
                        name: name.clone(),
                        detail: e.detail.clone(),
                        flaps: e.flaps.saturating_sub(1),
                        down_for: None,
                    });
                    e.announced_down = Some(e.changed_at);
                    e.last_down_alert = Some(now);
                    e.flaps = 0;
                }
            } else if e.up {
                if let Some(since) = e.announced_down.take() {
                    if alerts_for(name) {
                        out.push(Alert {
                            kind: AlertKind::Up,
                            name: name.clone(),
                            detail: e.detail.clone(),
                            flaps: e.flaps,
                            down_for: Some(e.changed_at.saturating_duration_since(since)),
                        });
                    }
                    e.flaps = 0;
                } else if !cooling {
                    // Stable again and nothing pending: old flips are moot.
                    e.flaps = 0;
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Names currently down, among those `watched`, sorted.
    #[must_use]
    pub fn down(&self, watched: impl Fn(&str) -> bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .iter()
            .filter(|(name, e)| !e.up && watched(name))
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Every service id seen and not removed.
    #[must_use]
    pub fn ids(&self) -> Vec<ServiceId> {
        self.names.keys().cloned().collect()
    }

    /// The next instant at which [`Tracker::due`] may return something.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(|e| {
                let settled = e.changed_at + self.settle;
                if !e.up && e.announced_down.is_none() && !e.silenced {
                    let cooled = e.last_down_alert.map(|t| t + self.cooldown);
                    Some(cooled.map_or(settled, |c| c.max(settled)))
                } else if e.up && e.announced_down.is_some() {
                    Some(settled)
                } else {
                    None
                }
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::service;

    const SETTLE: Duration = Duration::from_secs(30);
    const COOLDOWN: Duration = Duration::from_secs(15 * 60);

    struct Clock(Instant);

    impl Clock {
        fn at(&self, secs: u64) -> Instant {
            self.0 + Duration::from_secs(secs)
        }
    }

    fn setup() -> (Tracker, Clock) {
        (Tracker::new(SETTLE, COOLDOWN), Clock(Instant::now()))
    }

    fn svc(name: &str, state: ServiceState, exit: Option<i64>) -> Service {
        let mut s = service(name, state);
        s.exit_code = exit;
        s
    }

    fn up(name: &str) -> Service {
        svc(name, ServiceState::Running, None)
    }

    fn crashed(name: &str) -> Service {
        svc(name, ServiceState::Stopped, Some(1))
    }

    fn all(_: &str) -> bool {
        true
    }

    fn kinds(alerts: &[Alert]) -> Vec<(AlertKind, &str)> {
        alerts.iter().map(|a| (a.kind, a.detail.as_str())).collect()
    }

    #[test]
    fn classifies_states() {
        assert_eq!(classify(&up("a")), Some((true, "running".into())));
        assert_eq!(classify(&crashed("a")), Some((false, "exited 1".into())));
        let mut sick = up("a");
        sick.health = Health::Unhealthy;
        assert_eq!(classify(&sick), Some((false, "unhealthy".into())));
        assert_eq!(
            classify(&svc("a", ServiceState::Restarting, None)),
            Some((false, "restarting".into()))
        );
        assert_eq!(
            classify(&svc("a", ServiceState::Paused, None)),
            Some((true, "paused".into()))
        );
        assert_eq!(classify(&svc("a", ServiceState::Unknown, None)), None);
        let mut me = up("me");
        me.is_self = true;
        assert_eq!(classify(&me), None);
    }

    #[test]
    fn down_then_up_after_settling() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        assert!(t.due(c.at(100), all).is_empty(), "first sight is silent");

        t.upsert(&crashed("web"), c.at(100));
        assert_eq!(t.next_due(), Some(c.at(130)));
        assert!(t.due(c.at(129), all).is_empty(), "still settling");
        let alerts = t.due(c.at(130), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Down, "exited 1")]);
        assert_eq!(alerts[0].name, "web");
        assert!(t.due(c.at(500), all).is_empty(), "one per transition");
        assert_eq!(t.next_due(), None);

        t.upsert(&up("web"), c.at(600));
        let alerts = t.due(c.at(630), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Up, "running")]);
        assert_eq!(alerts[0].down_for, Some(Duration::from_secs(500)));
        assert!(t.due(c.at(5000), all).is_empty());
    }

    #[test]
    fn quick_restart_or_recreate_is_silent() {
        let (mut t, c) = setup();
        let old = up("web");
        t.upsert(&old, c.at(0));
        t.upsert(&crashed("web"), c.at(10));
        // `docker compose up` re-creates: old container removed, new one starts.
        t.remove(&old.id, c.at(12), all);
        let mut fresh = up("web");
        fresh.id = ServiceId::new(crate::model::ProviderKind::Docker, "web2");
        t.upsert(&fresh, c.at(15));
        assert!(t.due(c.at(100), all).is_empty());
        assert_eq!(t.next_due(), None);
    }

    #[test]
    fn crash_loop_is_rate_limited_and_counted() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.upsert(&crashed("web"), c.at(10));
        assert_eq!(t.due(c.at(40), all).len(), 1);

        // Comes back, stays up, crashes again a few times within the cooldown.
        t.upsert(&up("web"), c.at(60));
        assert_eq!(kinds(&t.due(c.at(90), all)), [(AlertKind::Up, "running")]);
        for i in 0..5 {
            t.upsert(&crashed("web"), c.at(100 + i * 20));
            t.upsert(&up("web"), c.at(105 + i * 20));
        }
        t.upsert(&crashed("web"), c.at(300));
        assert!(t.due(c.at(400), all).is_empty(), "cooling down");
        // The cooldown started with the first alert at 40.
        assert_eq!(t.next_due(), Some(c.at(40 + 900)));
        let alerts = t.due(c.at(940), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Down, "exited 1")]);
        assert_eq!(alerts[0].flaps, 5, "the five earlier crashes are mentioned");
    }

    #[test]
    fn flapping_during_cooldown_that_ends_up_is_not_mailed() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.upsert(&crashed("web"), c.at(10));
        t.due(c.at(40), all);
        t.upsert(&up("web"), c.at(50));
        t.due(c.at(80), all);
        t.upsert(&crashed("web"), c.at(100));
        t.upsert(&up("web"), c.at(200));
        assert!(t.due(c.at(2000), all).is_empty());
    }

    #[test]
    fn unwatched_or_disabled_is_never_reported() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.upsert(&crashed("web"), c.at(10));
        assert!(t.due(c.at(100), |_| false).is_empty());
        // Watching it now does not report the outage already under way...
        assert!(t.due(c.at(200), all).is_empty());
        assert_eq!(t.next_due(), None);
        // ...nor its end, but the next one.
        t.upsert(&up("web"), c.at(300));
        assert!(t.due(c.at(400), all).is_empty());
        t.upsert(&crashed("web"), c.at(500));
        assert_eq!(t.due(c.at(530), all).len(), 1);
    }

    #[test]
    fn first_seen_down_is_silent() {
        let (mut t, c) = setup();
        t.upsert(&crashed("old"), c.at(0));
        assert!(t.due(c.at(100), all).is_empty());
        t.upsert(&up("old"), c.at(200));
        assert!(t.due(c.at(300), all).is_empty());
    }

    #[test]
    fn operator_stop_is_silent() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.expect("web", c.at(100));
        t.upsert(&svc("web", ServiceState::Stopped, Some(0)), c.at(105));
        assert!(t.due(c.at(200), all).is_empty());
        t.upsert(&up("web"), c.at(300));
        assert!(
            t.due(c.at(400), all).is_empty(),
            "no recovery for a silent stop"
        );

        // After the window, a crash is reported again.
        t.upsert(&crashed("web"), c.at(500));
        assert_eq!(t.due(c.at(530), all).len(), 1);
    }

    #[test]
    fn removal_of_a_watched_service_is_down() {
        let (mut t, c) = setup();
        let web = up("web");
        let db = up("db");
        t.upsert(&web, c.at(0));
        t.upsert(&db, c.at(0));
        let watched = |n: &str| n == "web";
        t.remove(&web.id, c.at(10), watched);
        t.remove(&db.id, c.at(10), watched);
        let alerts = t.due(c.at(40), watched);
        assert_eq!(kinds(&alerts), [(AlertKind::Down, "removed")]);
        assert!(
            !t.entries.contains_key("db"),
            "unwatched names are forgotten"
        );
    }

    #[test]
    fn unhealthy_counts_as_down() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        let mut sick = up("web");
        sick.health = Health::Unhealthy;
        t.upsert(&sick, c.at(10));
        assert_eq!(
            kinds(&t.due(c.at(40), all)),
            [(AlertKind::Down, "unhealthy")]
        );
    }
}
