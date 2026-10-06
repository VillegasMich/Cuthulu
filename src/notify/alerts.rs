//! Turns registry changes into alerts. Pure: time is passed in, nothing is
//! sent from here.
//!
//! Rules, per service *name* (a re-created container keeps its alerts):
//!
//! - **Settle:** a change must last [`Tracker::new`]'s `settle` before it is
//!   reported as down / back up, so a `docker restart` or `docker compose up`
//!   re-creating the container stays silent.
//! - **One per transition:** "back up" is sent only after a "down" was.
//! - **Crashed and restarted:** a service that exits with a non-clean code
//!   (or dies) and is running again before the settle ends — a restart policy
//!   or a systemd `Restart=` at work — is reported right away, once.
//! - **Cooldown:** at most one "down" or "restarted" alert per service per
//!   `cooldown`. A crash loop gives one alert, then (if it is still down) one
//!   more when the cooldown ends, mentioning how often it went down. Stops
//!   requested through Cuthulu are exempt.
//! - **Operator actions:** a stop/restart requested through Cuthulu is
//!   reported like any outage, labelled as such; a service stopped through
//!   Cuthulu that comes back without a start from Cuthulu is flagged as
//!   started by something else (a systemd unit, a restart policy).
//! - Services first seen while down, and services not watched when they
//!   went down, are never reported for that outage.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::model::{Health, Service, ServiceId, ServiceState};

/// How long after an action requested through Cuthulu a down flip counts
/// as caused by it.
pub const ACTION_WINDOW: Duration = Duration::from_secs(60);
/// How long after a stop through Cuthulu a return counts as "started by
/// something else" (systemd's `RestartSec` is usually well below this).
pub const RETURN_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Exit codes of a normal `docker stop` (0, SIGINT, SIGKILL, SIGTERM); any
/// other code is a crash. Mirrors `CLEAN_EXIT` in `static/app.js`.
const CLEAN_EXIT: [i64; 4] = [0, 130, 137, 143];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Down,
    Up,
    /// Crashed and running again before the settle ended.
    Restarted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: AlertKind,
    pub name: String,
    /// State as text: `exited 1`, `unhealthy`, `removed`, `running`, …
    /// For [`AlertKind::Restarted`], how it crashed.
    pub detail: String,
    /// Down flips since the last alert that are not reported otherwise.
    pub flaps: u32,
    /// For [`AlertKind::Up`] / [`AlertKind::Restarted`]: how long it was down.
    pub down_for: Option<Duration>,
    /// For [`AlertKind::Down`]: stopped through Cuthulu.
    pub by_operator: bool,
    /// For [`AlertKind::Up`] / [`AlertKind::Restarted`]: stopped through
    /// Cuthulu, then started by something else.
    pub started_elsewhere: bool,
}

// Independent flags of one per-service state machine; an enum would not
// make them clearer.
#[allow(clippy::struct_excessive_bools)]
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
    /// Last action requested through Cuthulu: when, and whether a stop.
    action: Option<(Instant, bool)>,
    /// The current outage follows an action requested through Cuthulu.
    by_operator: bool,
    /// How the current outage began, when it was a crash.
    crash: Option<String>,
    /// A crash-and-restart waiting to be reported: `(crash, down for)`.
    restarted: Option<(String, Duration)>,
    /// The service came back on its own after a stop through Cuthulu.
    started_elsewhere: bool,
}

#[derive(Debug)]
pub struct Tracker {
    settle: Duration,
    cooldown: Duration,
    entries: HashMap<String, Entry>,
    /// Name of every service id currently known, for `Remove` events.
    names: HashMap<ServiceId, String>,
}

/// `(up, detail, crashed)` for a service, or `None` when it says nothing
/// about health: Cuthulu itself, or a state the provider could not read.
#[must_use]
pub fn classify(s: &Service) -> Option<(bool, String, bool)> {
    if s.is_self {
        return None;
    }
    let unhealthy = s.health == Health::Unhealthy;
    let up = matches!(s.state, ServiceState::Running | ServiceState::Paused) && !unhealthy;
    let crashed = match s.state {
        ServiceState::Dead | ServiceState::Restarting => true,
        ServiceState::Stopped => s.exit_code.is_some_and(|c| !CLEAN_EXIT.contains(&c)),
        _ => false,
    };
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
    Some((up, detail, crashed))
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
        let Some((up, detail, crashed)) = classify(s) else {
            return;
        };
        self.names.insert(s.id.clone(), s.name.clone());
        self.observe(&s.name, up, detail, crashed, now);
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
            self.observe(&name, false, "removed".to_owned(), false, now);
        } else {
            self.entries.remove(&name);
        }
    }

    /// The operator asked Cuthulu to act on `name` (`stop`: it was a stop).
    pub fn expect(&mut self, name: &str, stop: bool, now: Instant) {
        if let Some(e) = self.entries.get_mut(name) {
            e.action = Some((now, stop));
        }
    }

    fn observe(&mut self, name: &str, up: bool, detail: String, crashed: bool, now: Instant) {
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
                    action: None,
                    by_operator: false,
                    crash: None,
                    restarted: None,
                    started_elsewhere: false,
                },
            );
            return;
        };
        // While down, keep how it went down: a `--rm` container that exits
        // is "removed" next, its replacement "created" — neither is news.
        let lifecycle = !up && matches!(detail.as_str(), "removed" | "created");
        if !(e.up == up && lifecycle) {
            e.detail = detail;
        }
        if e.up == up {
            return;
        }
        e.up = up;
        let down_for = now.saturating_duration_since(e.changed_at);
        e.changed_at = now;
        if up {
            // Back after a stop through Cuthulu, with no start from it since.
            e.started_elsewhere = e.by_operator
                && e.action
                    .is_some_and(|(at, stop)| stop && now < at + RETURN_WINDOW);
            if let Some(crash) = e.crash.take()
                && e.announced_down.is_none()
                && !e.silenced
            {
                e.restarted = Some((crash, down_for));
            }
        } else {
            e.flaps += 1;
            e.silenced = false;
            e.started_elsewhere = false;
            e.by_operator = e.action.is_some_and(|(at, _)| now < at + ACTION_WINDOW);
            e.crash = (crashed && !e.by_operator).then(|| e.detail.clone());
        }
    }

    /// Alerts that are due at `now`. `alerts_for` says whether a name is
    /// watched (and alerts are switched on).
    pub fn due(&mut self, now: Instant, alerts_for: impl Fn(&str) -> bool) -> Vec<Alert> {
        let mut out = Vec::new();
        for (name, e) in &mut self.entries {
            let cooling = e.last_down_alert.is_some_and(|t| now < t + self.cooldown);
            // Judged at the moment it came back, so a late pass never sends
            // a stale one.
            let cooling_then = e
                .last_down_alert
                .is_some_and(|t| e.changed_at < t + self.cooldown);
            if let Some((crash, down_for)) = e.restarted.take()
                && alerts_for(name)
                && !cooling_then
            {
                out.push(Alert {
                    kind: AlertKind::Restarted,
                    name: name.clone(),
                    detail: crash,
                    flaps: e.flaps.saturating_sub(1),
                    down_for: Some(down_for),
                    by_operator: false,
                    started_elsewhere: e.started_elsewhere,
                });
                e.last_down_alert = Some(now);
                e.flaps = 0;
                continue;
            }
            if now < e.changed_at + self.settle {
                continue;
            }
            if !e.up && e.announced_down.is_none() && !e.silenced {
                if !alerts_for(name) {
                    e.silenced = true;
                } else if !cooling || e.by_operator {
                    out.push(Alert {
                        kind: AlertKind::Down,
                        name: name.clone(),
                        detail: e.detail.clone(),
                        flaps: e.flaps.saturating_sub(1),
                        down_for: None,
                        by_operator: e.by_operator,
                        started_elsewhere: false,
                    });
                    e.announced_down = Some(e.changed_at);
                    // Deliberate stops neither wait for nor start a cooldown.
                    if !e.by_operator {
                        e.last_down_alert = Some(now);
                    }
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
                            by_operator: false,
                            started_elsewhere: e.started_elsewhere,
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

    /// Running services stopped through Cuthulu and started by something
    /// else since, sorted.
    #[must_use]
    pub fn started_elsewhere(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.up && e.started_elsewhere)
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
                if e.restarted.is_some() {
                    // Reported (or dropped) on the next pass.
                    Some(e.changed_at)
                } else if !e.up && e.announced_down.is_none() && !e.silenced {
                    let cooled = e
                        .last_down_alert
                        .filter(|_| !e.by_operator)
                        .map(|t| t + self.cooldown);
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
        assert_eq!(classify(&up("a")), Some((true, "running".into(), false)));
        assert_eq!(
            classify(&crashed("a")),
            Some((false, "exited 1".into(), true))
        );
        for clean in [0, 130, 137, 143] {
            let s = svc("a", ServiceState::Stopped, Some(clean));
            assert_eq!(
                classify(&s),
                Some((false, format!("exited {clean}"), false))
            );
        }
        let mut sick = up("a");
        sick.health = Health::Unhealthy;
        assert_eq!(classify(&sick), Some((false, "unhealthy".into(), false)));
        assert_eq!(
            classify(&svc("a", ServiceState::Restarting, None)),
            Some((false, "restarting".into(), true))
        );
        assert_eq!(
            classify(&svc("a", ServiceState::Dead, None)),
            Some((false, "dead".into(), true))
        );
        assert_eq!(
            classify(&svc("a", ServiceState::Paused, None)),
            Some((true, "paused".into(), false))
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
        assert_eq!(t.due(c.at(5000), all), Vec::<Alert>::new());
    }

    #[test]
    fn quick_restart_or_recreate_is_silent() {
        let (mut t, c) = setup();
        let old = up("web");
        t.upsert(&old, c.at(0));
        t.upsert(&svc("web", ServiceState::Stopped, Some(0)), c.at(10));
        // `docker compose up` re-creates: old container removed, new one starts.
        t.remove(&old.id, c.at(12), all);
        let mut fresh = up("web");
        fresh.id = ServiceId::new(crate::model::ProviderKind::Docker, "web2");
        t.upsert(&fresh, c.at(15));
        assert_eq!(t.due(c.at(100), all), Vec::<Alert>::new());
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
        assert_eq!(t.due(c.at(2000), all), Vec::<Alert>::new());
    }

    #[test]
    fn unwatched_or_disabled_is_never_reported() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.upsert(&crashed("web"), c.at(10));
        assert_eq!(t.due(c.at(100), |_| false), Vec::<Alert>::new());
        // Watching it now does not report the outage already under way...
        assert_eq!(t.due(c.at(200), all), Vec::<Alert>::new());
        assert_eq!(t.next_due(), None);
        // ...nor its end, but the next one.
        t.upsert(&up("web"), c.at(300));
        assert_eq!(t.due(c.at(400), all), Vec::<Alert>::new());
        t.upsert(&crashed("web"), c.at(500));
        assert_eq!(t.due(c.at(530), all).len(), 1);
    }

    #[test]
    fn first_seen_down_is_silent() {
        let (mut t, c) = setup();
        t.upsert(&crashed("old"), c.at(0));
        assert_eq!(t.due(c.at(100), all), Vec::<Alert>::new());
        t.upsert(&up("old"), c.at(200));
        assert_eq!(t.due(c.at(300), all), Vec::<Alert>::new());
    }

    #[test]
    fn operator_stop_is_labelled() {
        let (mut t, c) = setup();
        t.upsert(&up("web"), c.at(0));
        t.expect("web", true, c.at(100));
        t.upsert(&svc("web", ServiceState::Stopped, Some(143)), c.at(105));
        let alerts = t.due(c.at(135), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Down, "exited 143")]);
        assert!(alerts[0].by_operator);

        // Started again through Cuthulu: a plain "back up".
        t.expect("web", false, c.at(200));
        t.upsert(&up("web"), c.at(202));
        let alerts = t.due(c.at(232), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Up, "running")]);
        assert!(!alerts[0].started_elsewhere);

        // Stopping again soon after is still reported: no cooldown for
        // deliberate stops.
        t.expect("web", true, c.at(300));
        t.upsert(&svc("web", ServiceState::Stopped, Some(0)), c.at(301));
        assert!(t.due(c.at(331), all)[0].by_operator);
        t.upsert(&up("web"), c.at(400));
        t.due(c.at(430), all);

        // Much later, a crash is not the operator's doing.
        t.upsert(&crashed("web"), c.at(2000));
        let alerts = t.due(c.at(2030), all);
        assert!(!alerts[0].by_operator);
    }

    #[test]
    fn stopped_here_but_started_by_something_else() {
        // A systemd unit with Restart=always and RestartSec=60.
        let (mut t, c) = setup();
        let old = up("web");
        t.upsert(&old, c.at(0));
        t.expect("web", true, c.at(100));
        t.upsert(&svc("web", ServiceState::Stopped, Some(143)), c.at(101));
        t.remove(&old.id, c.at(101), all);
        assert_eq!(
            kinds(&t.due(c.at(131), all)),
            [(AlertKind::Down, "exited 143")],
            "the cause, not the removal"
        );
        let mut fresh = up("web");
        fresh.id = ServiceId::new(crate::model::ProviderKind::Docker, "web2");
        t.upsert(&fresh, c.at(161));
        let alerts = t.due(c.at(191), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Up, "running")]);
        assert!(alerts[0].started_elsewhere);
        assert_eq!(t.started_elsewhere(), ["web"]);
        // Until it goes down again.
        t.upsert(&svc("web", ServiceState::Stopped, Some(0)), c.at(300));
        assert_eq!(t.started_elsewhere(), Vec::<String>::new());
    }

    #[test]
    fn crash_with_quick_restart_is_reported_once() {
        let (mut t, c) = setup();
        let old = up("web");
        t.upsert(&old, c.at(0));
        // `docker run --rm` under systemd: exit 3, removed, re-created.
        t.upsert(&svc("web", ServiceState::Stopped, Some(3)), c.at(100));
        t.remove(&old.id, c.at(100), all);
        let mut fresh = svc("web", ServiceState::Created, None);
        fresh.id = ServiceId::new(crate::model::ProviderKind::Docker, "web2");
        t.upsert(&fresh, c.at(110));
        fresh.state = ServiceState::Running;
        t.upsert(&fresh, c.at(111));
        assert_eq!(t.next_due(), Some(c.at(111)));
        let alerts = t.due(c.at(111), all);
        assert_eq!(kinds(&alerts), [(AlertKind::Restarted, "exited 3")]);
        assert_eq!(alerts[0].down_for, Some(Duration::from_secs(11)));
        assert_eq!(t.due(c.at(500), all), Vec::<Alert>::new());

        // Again within the cooldown: counted, not mailed.
        t.upsert(&crashed("web"), c.at(600));
        t.upsert(&up("web"), c.at(605));
        assert_eq!(t.due(c.at(605), all), Vec::<Alert>::new());
        // Unwatched: nothing either.
        t.upsert(&crashed("web"), c.at(5000));
        t.upsert(&up("web"), c.at(5005));
        assert_eq!(t.due(c.at(5005), |_| false), Vec::<Alert>::new());
        // A clean stop that comes back quickly is a restart, not a crash.
        t.upsert(&svc("web", ServiceState::Stopped, Some(0)), c.at(9000));
        t.upsert(&up("web"), c.at(9005));
        assert_eq!(t.due(c.at(9100), all), Vec::<Alert>::new());
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
