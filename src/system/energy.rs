//! CPU package power and energy from the RAPL counters.
//!
//! `energy_uj` is a cumulative counter that wraps at `max_energy_range_uj`
//! (about 18 Wh on a 15 W laptop: every 1.6 h at full load). Power is the
//! energy between two reads over the time between them; the energy total
//! needs every wrap seen, so a light tick reads the counters at least twice
//! per wrap period even when nobody watches (see [`Energy::tick_interval`]).
//! The host panel's sampler reads through the same [`Energy`], so both feed
//! one total and never see each other's reads as a wrap.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::sysfs::{self, Package, RaplStatus};

/// Longest gap between two reads of the always-on tick.
pub const MAX_TICK: Duration = Duration::from_secs(60);
/// Shortest gap, whatever the wrap period says.
const MIN_TICK: Duration = Duration::from_secs(1);
/// Watts are recomputed only over windows at least this long, so a read
/// right after another one (the tick and the sampler) does not give a noisy
/// value.
const MIN_WINDOW: Duration = Duration::from_millis(500);
/// The highest-seen scale is rounded up to a multiple of this many watts,
/// so the meter is not pinned at 100 % by the value that set it.
pub const PEAK_STEP: f64 = 5.0;
/// A delta implying more than this (per package) is a counter reset, e.g.
/// after a resume, and is dropped rather than counted.
const MAX_PLAUSIBLE_W: f64 = 5_000.0;

/// Where the Pwr meter's full scale comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    /// The package's `long_term` (PL1) power limit.
    Rapl,
    /// `CUTHULU_POWER_MAX_W`.
    Config,
    /// The highest power seen since Cuthulu started, rounded up to
    /// [`PEAK_STEP`] watts. Not a limit, so the meter shows no alarm zones.
    Peak,
}

/// Power and energy as the snapshot carries them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Power {
    pub status: RaplStatus,
    /// Package power summed over packages, W.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watts: Option<f64>,
    /// The cores' share of it, W, when the CPU reports a core domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_watts: Option<f64>,
    /// The Pwr meter's full scale, W.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_watts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
    /// Package energy since Cuthulu started (in memory only), Wh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_wh: Option<f64>,
    /// Time that energy covers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub energy_secs: Option<u64>,
}

/// One read of every counter: `(value, wraps at)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub packages: Vec<(u64, u64)>,
    pub cores: Vec<(u64, u64)>,
}

/// Energy from `prev` to `now` of a counter that wraps at `max`. A counter
/// that went back without a known range is taken as reset: nothing counted.
#[must_use]
pub fn delta(prev: u64, now: u64, max: u64) -> u64 {
    if now >= prev {
        now - prev
    } else if max >= prev {
        max - prev + now
    } else {
        0
    }
}

/// Totals over every read since the first. Pure: time comes in with each
/// reading, so the tests drive it with made-up instants.
#[derive(Debug, Clone, Default)]
pub struct Accumulator {
    first: Option<Instant>,
    prev: Option<(Instant, Reading)>,
    /// µJ since the first read.
    package_uj: u128,
    core_uj: u128,
    /// Start of the current power window: (when, package µJ, core µJ).
    window: Option<(Instant, u128, u128)>,
    watts: Option<f64>,
    core_watts: Option<f64>,
    peak: f64,
}

impl Accumulator {
    pub fn record(&mut self, at: Instant, now: Reading) {
        self.first.get_or_insert(at);
        if let Some((then, prev)) = &self.prev {
            let secs = at.saturating_duration_since(*then).as_secs_f64();
            self.package_uj += sum_deltas(&prev.packages, &now.packages, secs);
            self.core_uj += sum_deltas(&prev.cores, &now.cores, secs);
        }
        self.prev = Some((at, now));

        let (since, pkg, core) = *self
            .window
            .get_or_insert((at, self.package_uj, self.core_uj));
        let elapsed = at.saturating_duration_since(since);
        if elapsed >= MIN_WINDOW {
            let secs = elapsed.as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let watts = |uj: u128| uj as f64 / 1e6 / secs;
            let w = watts(self.package_uj - pkg);
            self.watts = Some(w);
            self.peak = self.peak.max(w);
            let has_core = self.prev.as_ref().is_some_and(|(_, r)| !r.cores.is_empty());
            self.core_watts = has_core.then(|| watts(self.core_uj - core));
            self.window = Some((at, self.package_uj, self.core_uj));
        }
    }

    /// Current package power, W.
    #[must_use]
    pub fn watts(&self) -> Option<f64> {
        self.watts
    }

    #[must_use]
    pub fn core_watts(&self) -> Option<f64> {
        self.core_watts
    }

    /// Highest package power seen, W (0 before the first value).
    #[must_use]
    pub fn peak(&self) -> f64 {
        self.peak
    }

    /// Energy since the first read, Wh, and the time it covers.
    #[must_use]
    pub fn energy(&self) -> Option<(f64, Duration)> {
        let first = self.first?;
        let (last, _) = self.prev.as_ref()?;
        #[allow(clippy::cast_precision_loss)]
        let wh = self.package_uj as f64 / 3.6e9;
        Some((wh, last.saturating_duration_since(first)))
    }
}

fn sum_deltas(prev: &[(u64, u64)], now: &[(u64, u64)], secs: f64) -> u128 {
    prev.iter()
        .zip(now)
        .map(|(&(p, _), &(n, max))| delta(p, n, max))
        // A jump no CPU could draw in that time is a reset, not energy.
        .filter(|&d| {
            #[allow(clippy::cast_precision_loss)]
            let w = d as f64 / 1e6 / secs.max(1e-3);
            w <= MAX_PLAUSIBLE_W
        })
        .map(u128::from)
        .sum()
}

/// The RAPL counters of this machine and what has been read from them.
#[derive(Debug)]
pub struct Energy {
    status: RaplStatus,
    packages: Vec<Package>,
    /// Sum of the packages' `long_term` limits, W.
    limit: Option<f64>,
    /// `CUTHULU_POWER_MAX_W`.
    config_max: Option<f64>,
    acc: Mutex<Accumulator>,
}

impl Energy {
    /// Looks for RAPL once; its status holds for the process's lifetime.
    #[must_use]
    pub fn probe(powercap: &Path, sys: &Path, config_max: Option<f64>) -> Self {
        let (status, packages) = sysfs::probe_rapl(powercap, &sys.join("class/powercap"));
        let limits: Option<Vec<u64>> = packages.iter().map(|p| p.limit_uw).collect();
        #[allow(clippy::cast_precision_loss)]
        let limit = limits
            .filter(|l| !l.is_empty())
            .map(|l| l.iter().sum::<u64>() as f64 / 1e6);
        Self {
            status,
            packages,
            limit,
            config_max,
            acc: Mutex::default(),
        }
    }

    #[must_use]
    pub fn status(&self) -> RaplStatus {
        self.status
    }

    /// Whether the counters can be read, i.e. the tick has work to do.
    #[must_use]
    pub fn readable(&self) -> bool {
        self.status == RaplStatus::Ok
    }

    /// Reads every counter and folds it into the totals. Blocking (small
    /// sysfs reads); a no-op when RAPL is not readable.
    ///
    /// # Errors
    /// The first counter that could not be read.
    pub fn read(&self) -> std::io::Result<()> {
        if !self.readable() {
            return Ok(());
        }
        // Read under the lock: two readers folding their values out of
        // order would look like a wrap.
        let mut acc = self.acc();
        let mut reading = Reading {
            packages: Vec::with_capacity(self.packages.len()),
            cores: Vec::new(),
        };
        for p in &self.packages {
            reading
                .packages
                .push((sysfs::read_counter(&p.energy.path)?, p.energy.max));
        }
        // Only when every package has one, so the sum is comparable.
        if self.packages.iter().all(|p| p.core.is_some()) {
            for c in self.packages.iter().filter_map(|p| p.core.as_ref()) {
                reading.cores.push((sysfs::read_counter(&c.path)?, c.max));
            }
        }
        acc.record(Instant::now(), reading);
        Ok(())
    }

    /// The snapshot's view: status, watts, scale and energy so far.
    #[must_use]
    pub fn power(&self) -> Power {
        let acc = self.acc();
        let (scale_watts, scale) = match (self.limit, self.config_max) {
            (Some(w), _) => (Some(w), Some(Scale::Rapl)),
            (None, Some(w)) => (Some(w), Some(Scale::Config)),
            (None, None) if self.readable() => (Some(peak_scale(acc.peak())), Some(Scale::Peak)),
            (None, None) => (None, None),
        };
        let energy = acc.energy();
        Power {
            status: self.status,
            watts: acc.watts().map(round1),
            core_watts: acc.core_watts().map(round1),
            scale_watts: scale_watts.map(round1),
            scale,
            energy_wh: energy.map(|(wh, _)| (wh * 1000.0).round() / 1000.0),
            energy_secs: energy.map(|(_, d)| d.as_secs()),
        }
    }

    /// Gap until the next tick: [`MAX_TICK`], or less when the counters
    /// could wrap sooner at the highest power known (now, peak or limit),
    /// so every wrap period has at least two reads.
    #[must_use]
    pub fn tick_interval(&self) -> Duration {
        let acc = self.acc();
        let watts = [acc.watts(), Some(acc.peak()), self.limit]
            .into_iter()
            .flatten()
            .fold(0.0_f64, f64::max);
        let range = self
            .packages
            .iter()
            .map(|p| p.energy.max)
            .filter(|m| *m > 0)
            .min();
        tick_interval(range, watts)
    }

    fn acc(&self) -> MutexGuard<'_, Accumulator> {
        self.acc.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The highest-seen scale: `peak` rounded up to the next multiple of
/// [`PEAK_STEP`], and at least one step (no reading yet: 5 W).
#[must_use]
pub fn peak_scale(peak: f64) -> f64 {
    if peak.is_finite() && peak > 0.0 {
        ((peak / PEAK_STEP).ceil() * PEAK_STEP).max(PEAK_STEP)
    } else {
        PEAK_STEP
    }
}

/// Half the time `range_uj` takes to fill at `watts`, within
/// one second and [`MAX_TICK`].
#[must_use]
pub fn tick_interval(range_uj: Option<u64>, watts: f64) -> Duration {
    let Some(range) = range_uj else {
        return MAX_TICK;
    };
    if watts <= 0.0 {
        return MAX_TICK;
    }
    #[allow(clippy::cast_precision_loss)]
    let wrap = range as f64 / 1e6 / watts;
    Duration::from_secs_f64((wrap / 2.0).clamp(MIN_TICK.as_secs_f64(), MAX_TICK.as_secs_f64()))
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::sysfs::tests::rapl;
    use crate::system::tests::FakeProc;

    const MAX: u64 = 65_532_610_987;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn pkg(v: u64) -> Reading {
        Reading {
            packages: vec![(v, MAX)],
            cores: vec![],
        }
    }

    #[test]
    fn delta_handles_wraparound() {
        assert_eq!(delta(100, 250, MAX), 150);
        assert_eq!(delta(MAX - 50, 100, MAX), 150, "wrapped once");
        assert_eq!(delta(500, 100, 0), 0, "unknown range: a reset, not energy");
        assert_eq!(delta(7, 7, MAX), 0);
    }

    #[test]
    fn watts_and_energy_across_a_wrap() {
        let t0 = Instant::now();
        let mut acc = Accumulator::default();
        // 10.9 W for 60 s = 654 J per step, starting just below the wrap.
        let step = 654_000_000;
        let start = MAX - 300_000_000;
        acc.record(t0, pkg(start));
        assert_eq!(acc.watts(), None, "one read is no rate");
        acc.record(at(t0, 60_000), pkg((start + step) % MAX));
        let w = acc.watts().unwrap();
        assert!((w - 10.9).abs() < 1e-6, "{w}");
        acc.record(at(t0, 120_000), pkg((start + 2 * step) % MAX));
        let (wh, d) = acc.energy().unwrap();
        // 1308 J = 0.3633 Wh over two minutes.
        assert!((wh - 1308.0 / 3600.0).abs() < 1e-9, "{wh}");
        assert_eq!(d, Duration::from_secs(120));
        assert!((acc.peak() - 10.9).abs() < 1e-6);
    }

    #[test]
    fn packages_sum_and_cores_are_reported() {
        let t0 = Instant::now();
        let mut acc = Accumulator::default();
        let r = |a, b, c| Reading {
            packages: vec![(a, MAX), (b, MAX)],
            cores: vec![(c, MAX), (c, MAX)],
        };
        acc.record(t0, r(0, 0, 0));
        acc.record(at(t0, 2000), r(10_000_000, 4_000_000, 1_000_000));
        assert_eq!(acc.watts(), Some(7.0));
        assert_eq!(acc.core_watts(), Some(1.0));
    }

    #[test]
    fn close_reads_keep_the_last_watts_but_count_energy() {
        let t0 = Instant::now();
        let mut acc = Accumulator::default();
        acc.record(t0, pkg(0));
        acc.record(at(t0, 2000), pkg(20_000_000));
        assert_eq!(acc.watts(), Some(10.0));
        // The tick lands 10 ms after the sampler.
        acc.record(at(t0, 2010), pkg(20_500_000));
        assert_eq!(acc.watts(), Some(10.0), "too short a window to say");
        acc.record(at(t0, 4000), pkg(40_000_000));
        let w = acc.watts().unwrap();
        assert!((w - 10.0).abs() < 1e-9, "window since the last value: {w}");
        assert!((acc.energy().unwrap().0 - 40.0 / 3600.0).abs() < 1e-12);
    }

    #[test]
    fn implausible_jumps_are_resets() {
        let t0 = Instant::now();
        let mut acc = Accumulator::default();
        acc.record(t0, pkg(1_000));
        // Counter restarted near the top of its range: not ~18 Wh in 2 s.
        acc.record(at(t0, 2000), pkg(MAX - 10));
        assert_eq!(acc.energy().unwrap().0, 0.0);
        assert_eq!(acc.watts(), Some(0.0));
    }

    #[test]
    fn peak_scale_rounds_up_to_five_watts() {
        assert_eq!(peak_scale(12.4), 15.0);
        assert_eq!(peak_scale(15.0), 15.0, "already a multiple");
        assert_eq!(peak_scale(15.01), 20.0);
        assert_eq!(peak_scale(0.3), 5.0);
        assert_eq!(peak_scale(0.0), 5.0, "no reading yet");
        assert_eq!(peak_scale(f64::NAN), 5.0);
        assert_eq!(peak_scale(97.0), 100.0);
    }

    #[test]
    fn tick_reads_twice_per_wrap() {
        assert_eq!(
            tick_interval(Some(MAX), 15.0),
            MAX_TICK,
            "wraps every ~73 min"
        );
        // 100 J range at 10 W: wraps every 10 s, so read every 5 s.
        assert_eq!(
            tick_interval(Some(100_000_000), 10.0),
            Duration::from_secs(5)
        );
        assert_eq!(tick_interval(Some(1_000), 500.0), MIN_TICK);
        assert_eq!(tick_interval(None, 10.0), MAX_TICK);
        assert_eq!(tick_interval(Some(MAX), 0.0), MAX_TICK);
    }

    #[test]
    fn energy_reads_the_counters_and_picks_a_scale() {
        let fake = FakeProc::new();
        rapl(&fake, "sys/class/powercap", 1_000);
        let sys = fake.0.join("sys");
        let class = sys.join("class/powercap");
        let e = Energy::probe(&class, &sys, Some(25.0));
        assert!(e.readable());
        e.read().unwrap();
        let p = e.power();
        assert_eq!(p.status, RaplStatus::Ok);
        assert_eq!((p.scale, p.scale_watts), (Some(Scale::Config), Some(25.0)));
        assert_eq!(p.energy_wh, Some(0.0));

        fake.write(
            "sys/class/powercap/intel-rapl:0/constraint_0_name",
            "long_term\n",
        );
        fake.write(
            "sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
            "15000000\n",
        );
        let e = Energy::probe(&class, &sys, Some(25.0));
        let p = e.power();
        assert_eq!((p.scale, p.scale_watts), (Some(Scale::Rapl), Some(15.0)));
        assert_eq!(p.watts, None);

        std::fs::remove_file(class.join("intel-rapl:0/constraint_0_name")).unwrap();
        let e = Energy::probe(&class, &sys, None);
        assert_eq!(
            (e.power().scale, e.power().scale_watts),
            (Some(Scale::Peak), Some(5.0)),
            "no reading yet"
        );
        e.acc().peak = 12.34;
        assert_eq!(
            (e.power().scale, e.power().scale_watts),
            (Some(Scale::Peak), Some(15.0))
        );
    }

    #[test]
    fn unreadable_rapl_reads_nothing() {
        let fake = FakeProc::new();
        let sys = fake.0.join("sys");
        let e = Energy::probe(&sys.join("class/powercap"), &sys, None);
        assert_eq!(e.status(), RaplStatus::Absent);
        e.read().unwrap();
        assert_eq!(
            e.power(),
            Power {
                status: RaplStatus::Absent,
                watts: None,
                core_watts: None,
                scale_watts: None,
                scale: None,
                energy_wh: None,
                energy_secs: None,
            }
        );
        let json = serde_json::to_value(e.power()).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "absent" }));
    }
}
