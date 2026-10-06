//! Host system stats (CPU, memory, load, top processes) read from `/proc`.
//!
//! This is not a [`Provider`](crate::providers::Provider): the host is not a
//! service source. One shared sampler runs only while at least one client is
//! subscribed and stops on its own when the last one leaves, so nobody
//! watching costs nothing. Results go out on a small bounded broadcast;
//! every update is a full snapshot, so a lagging subscriber just skips some.

pub mod proc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use self::proc::{LoadAvg, MemInfo, PidStat, PidStatus, Stat, ratio};

/// Processes listed per sort key. The snapshot carries the union of the top
/// `TOP` by CPU and by memory, so the client can sort by either.
pub const TOP: usize = 10;
/// Longest command line sent per process, in bytes.
const MAX_CMD: usize = 512;
/// Snapshots buffered per subscriber; older ones are skipped, never queued.
const CAPACITY: usize = 4;
/// Gap between the two reads of a one-off sample (CPU% needs a delta).
const BASELINE: Duration = Duration::from_millis(250);
const PASSWD: &str = "/etc/passwd";

#[derive(Debug, Clone, thiserror::Error)]
pub enum SystemError {
    #[error("cannot read {path}: {message}")]
    Io { path: String, message: String },
    #[error(transparent)]
    Parse(#[from] proc::ParseError),
    #[error("sampler task failed: {0}")]
    Task(String),
}

/// What subscribers receive: a snapshot, or why there is none.
pub type Update = Result<Arc<Snapshot>, SystemError>;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub hostname: Option<String>,
    /// Aggregate CPU busy %, 0–100.
    pub cpu: f64,
    /// Busy % per online core, in core order.
    pub cpus: Vec<f64>,
    pub mem: Usage,
    pub swap: Usage,
    /// 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    /// Processes visible under the proc dir (fewer with `hidepid`).
    pub tasks: u32,
    pub threads: u32,
    /// Runnable threads (from `/proc/loadavg`).
    pub running: u32,
    pub uptime_secs: u64,
    /// How many processes the client should list.
    pub top: usize,
    /// Highest CPU first.
    pub procs: Vec<Process>,
}

/// Bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub total: u64,
    pub used: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Process {
    pub pid: u32,
    pub uid: u32,
    /// From `/etc/passwd` when it knows the uid.
    pub user: Option<String>,
    /// % of one core, as in htop (can exceed 100 for multi-threaded processes).
    pub cpu: f64,
    /// % of total memory.
    pub mem: f64,
    /// Resident set size in bytes.
    pub rss: u64,
    /// Command line, or `[comm]` for kernel threads.
    pub cmd: String,
}

/// Per-process counters from one read of the proc dir.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcRaw {
    stat: PidStat,
    status: PidStatus,
}

/// Everything one CPU% delta needs, from one point in time.
#[derive(Debug, Clone)]
struct Raw {
    at: Instant,
    stat: Stat,
    procs: HashMap<u32, ProcRaw>,
}

#[derive(Default)]
struct State {
    running: bool,
    latest: Option<(Instant, Update)>,
}

pub struct SystemMonitor {
    proc_dir: PathBuf,
    interval: Duration,
    users: HashMap<u32, String>,
    tx: broadcast::Sender<Update>,
    state: Mutex<State>,
    /// Previous raw read, the baseline for the next CPU% delta.
    prev: Mutex<Option<Raw>>,
    shutdown: CancellationToken,
}

impl SystemMonitor {
    #[must_use]
    pub fn new(proc_dir: PathBuf, interval: Duration, shutdown: CancellationToken) -> Arc<Self> {
        // Optional: in the container the host's file is mounted read-only;
        // without it users are shown by uid.
        let users = std::fs::read_to_string(PASSWD)
            .map(|s| proc::parse_passwd(&s))
            .unwrap_or_default();
        Arc::new(Self {
            proc_dir,
            interval,
            users,
            tx: broadcast::channel(CAPACITY).0,
            state: Mutex::default(),
            prev: Mutex::default(),
            shutdown,
        })
    }

    /// Subscribes to live snapshots, starting the shared sampler if it is
    /// not running. It stops by itself once every receiver is dropped.
    pub fn subscribe(self: &Arc<Self>) -> broadcast::Receiver<Update> {
        // `running` is only read and written under this lock, together with
        // the receiver count, so a subscriber arriving while the sampler
        // decides to stop is never left without one.
        let mut st = self.state();
        let rx = self.tx.subscribe();
        if !st.running {
            st.running = true;
            debug!("host sampler starting");
            tokio::spawn(Arc::clone(self).run());
        }
        rx
    }

    /// Whether the background sampler is currently running.
    #[must_use]
    pub fn is_sampling(&self) -> bool {
        self.state().running
    }

    /// The last update, if it is recent enough to still be current.
    #[must_use]
    pub fn latest(&self) -> Option<Update> {
        let st = self.state();
        let (at, update) = st.latest.as_ref()?;
        (at.elapsed() <= self.interval + Duration::from_secs(1)).then(|| update.clone())
    }

    /// The current snapshot: the sampler's latest, or a fresh one-off sample.
    pub async fn snapshot(self: &Arc<Self>) -> Update {
        if let Some(u) = self.latest() {
            return u;
        }
        let update = self.sample().await;
        self.state().latest = Some((Instant::now(), update.clone()));
        update
    }

    async fn run(self: Arc<Self>) {
        let mut failing = false;
        loop {
            let update = self.sample().await;
            match &update {
                Err(e) if !failing => warn!(error = %e, "host stats unavailable"),
                Ok(_) if failing => debug!("host stats available again"),
                _ => {}
            }
            failing = update.is_err();
            self.state().latest = Some((Instant::now(), update.clone()));
            let _ = self.tx.send(update);

            let stop = tokio::select! {
                () = tokio::time::sleep(self.interval) => false,
                () = self.shutdown.cancelled() => true,
            };
            let mut st = self.state();
            if stop || self.tx.receiver_count() == 0 {
                st.running = false;
                debug!("host sampler stopped");
                return;
            }
        }
    }

    async fn sample(self: &Arc<Self>) -> Update {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.sample_blocking().map(Arc::new))
            .await
            .unwrap_or_else(|e| Err(SystemError::Task(e.to_string())))
    }

    fn sample_blocking(&self) -> Result<Snapshot, SystemError> {
        let mut prev = self.prev.lock().unwrap_or_else(PoisonError::into_inner);
        let stale = (self.interval * 3).max(Duration::from_secs(5));
        let base = match prev.take() {
            Some(p) if p.at.elapsed() <= stale => p,
            _ => {
                let base = self.read_raw()?;
                std::thread::sleep(BASELINE.min(self.interval));
                base
            }
        };
        let now = self.read_raw()?;
        let mem = proc::parse_meminfo(&self.read("meminfo")?)?;
        let load = proc::parse_loadavg(&self.read("loadavg")?)?;
        let uptime = proc::parse_uptime(&self.read("uptime")?)?;
        let hostname = self
            .read("sys/kernel/hostname")
            .ok()
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty());

        let host = Host {
            hostname,
            mem,
            load,
            uptime,
        };
        let snap = build(&base, &now, host, &self.users, |pid| {
            std::fs::read(self.proc_dir.join(pid.to_string()).join("cmdline"))
                .map(|raw| proc::parse_cmdline(&raw, MAX_CMD))
                .unwrap_or_default()
        });
        *prev = Some(now);
        Ok(snap)
    }

    fn read(&self, rel: &str) -> Result<String, SystemError> {
        let path = self.proc_dir.join(rel);
        std::fs::read_to_string(&path).map_err(|e| io_err(&path, &e))
    }

    fn read_raw(&self) -> Result<Raw, SystemError> {
        let at = Instant::now();
        let stat = proc::parse_stat(&self.read("stat")?)?;
        let dir = std::fs::read_dir(&self.proc_dir).map_err(|e| io_err(&self.proc_dir, &e))?;
        let mut procs = HashMap::new();
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            // Processes vanish between listing and reading, and `hidepid`
            // hides other users' ones: skip anything unreadable.
            if let Some(p) = read_proc(&entry.path()) {
                procs.insert(pid, p);
            }
        }
        Ok(Raw { at, stat, procs })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn read_proc(dir: &Path) -> Option<ProcRaw> {
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    Some(ProcRaw {
        stat: proc::parse_pid_stat(&stat).ok()?,
        status: proc::parse_pid_status(&status).ok()?,
    })
}

fn io_err(path: &Path, e: &std::io::Error) -> SystemError {
    SystemError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// Host-wide values that need no delta.
struct Host {
    hostname: Option<String>,
    mem: MemInfo,
    load: LoadAvg,
    uptime: u64,
}

/// Turns two raw reads into a snapshot. Pure apart from `cmdline`, which
/// is only called for the processes that make the top lists.
fn build(
    prev: &Raw,
    now: &Raw,
    host: Host,
    users: &HashMap<u32, String>,
    cmdline: impl Fn(u32) -> String,
) -> Snapshot {
    let cpus = now
        .stat
        .cores
        .iter()
        .map(|(id, t)| {
            let before = prev
                .stat
                .cores
                .iter()
                .find(|(p, _)| p == id)
                .map_or(*t, |(_, b)| *b);
            round1(t.percent_since(before))
        })
        .collect();

    // A process's CPU% is relative to one core, like htop: its ticks over
    // the wall-clock ticks elapsed, which is the aggregate delta per core.
    let cores = now.stat.cores.len().max(1) as u64;
    let elapsed = now.stat.all.total.saturating_sub(prev.stat.all.total);
    let proc_cpu = |pid: u32, p: &ProcRaw| -> f64 {
        let before = prev
            .procs
            .get(&pid)
            .filter(|b| b.stat.start == p.stat.start)
            // New since the last read: no baseline yet.
            .map_or(p.stat.ticks, |b| b.stat.ticks);
        ratio(p.stat.ticks.saturating_sub(before) * cores, elapsed) * 100.0
    };

    let all: Vec<(u32, &ProcRaw, f64)> = now
        .procs
        .iter()
        .map(|(pid, p)| (*pid, p, proc_cpu(*pid, p)))
        .collect();

    // Highest first; ties broken by the other key, then by pid.
    let rss = |i: usize| all[i].1.status.rss;
    let mut by_cpu: Vec<usize> = (0..all.len()).collect();
    by_cpu.sort_by(|&a, &b| {
        (all[b].2.total_cmp(&all[a].2))
            .then(rss(b).cmp(&rss(a)))
            .then(all[a].0.cmp(&all[b].0))
    });
    let mut by_mem = by_cpu.clone();
    by_mem.sort_by(|&a, &b| {
        (rss(b).cmp(&rss(a)))
            .then(all[b].2.total_cmp(&all[a].2))
            .then(all[a].0.cmp(&all[b].0))
    });
    let mut top: Vec<usize> = by_cpu.into_iter().take(TOP).collect();
    for i in by_mem.into_iter().take(TOP) {
        if !top.contains(&i) {
            top.push(i);
        }
    }

    let procs = top
        .into_iter()
        .map(|i| {
            let (pid, p, cpu) = all[i];
            let cmd = cmdline(pid);
            Process {
                pid,
                uid: p.status.uid,
                user: users.get(&p.status.uid).cloned(),
                cpu: round1(cpu),
                mem: round1(ratio(p.status.rss, host.mem.total) * 100.0),
                rss: p.status.rss,
                cmd: if cmd.is_empty() {
                    format!("[{}]", p.stat.comm)
                } else {
                    cmd
                },
            }
        })
        .collect();

    Snapshot {
        hostname: host.hostname,
        cpu: round1(now.stat.all.percent_since(prev.stat.all)),
        cpus,
        mem: Usage {
            total: host.mem.total,
            used: host.mem.used,
        },
        swap: Usage {
            total: host.mem.swap_total,
            used: host.mem.swap_used,
        },
        load: [host.load.one, host.load.five, host.load.fifteen],
        tasks: u32::try_from(now.procs.len()).unwrap_or(u32::MAX),
        threads: host.load.threads,
        running: host.load.running,
        uptime_secs: host.uptime,
        top: TOP,
        procs,
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::proc::CpuTimes;
    use super::*;

    /// A fake proc dir under the system temp dir, removed on drop.
    pub(crate) struct FakeProc(pub PathBuf);

    impl FakeProc {
        pub(crate) fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "cuthulu-proc-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(dir.join("sys/kernel")).unwrap();
            let fake = Self(dir);
            fake.write(
                "meminfo",
                "MemTotal: 1000 kB\nMemAvailable: 250 kB\nSwapTotal: 100 kB\nSwapFree: 100 kB\n",
            );
            fake.write("loadavg", "0.50 0.25 0.10 1/40 99\n");
            fake.write("uptime", "3600.5 100.0\n");
            fake.write("sys/kernel/hostname", "box\n");
            fake.cpu(0, 0);
            fake.pid(1, "init", 0, 100, b"/sbin/init\0");
            fake.pid(2, "kthreadd", 0, 0, b"");
            fake
        }

        pub(crate) fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        /// Two cores; `busy` and `idle` ticks per core.
        pub(crate) fn cpu(&self, busy: u64, idle: u64) {
            self.write(
                "stat",
                format!(
                    "cpu  {} 0 0 {} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {idle} 0 0 0 0 0 0\ncpu1 {busy} 0 0 {idle} 0 0 0 0 0 0\n",
                    busy * 2,
                    idle * 2
                ),
            );
        }

        pub(crate) fn pid(&self, pid: u32, comm: &str, ticks: u64, rss_kb: u64, cmdline: &[u8]) {
            self.write(
                &format!("{pid}/stat"),
                format!("{pid} ({comm}) S 0 0 0 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 7 0 0"),
            );
            self.write(
                &format!("{pid}/status"),
                format!("Name:\t{comm}\nUid:\t0\t0\t0\t0\nVmRSS:\t{rss_kb} kB\n"),
            );
            self.write(&format!("{pid}/cmdline"), cmdline);
        }
    }

    impl Drop for FakeProc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn raw(all: CpuTimes, procs: &[(u32, u64, u64)]) -> Raw {
        Raw {
            at: Instant::now(),
            stat: Stat {
                all,
                cores: vec![(0, all), (1, all)],
            },
            procs: procs
                .iter()
                .map(|&(pid, ticks, rss)| {
                    let p = ProcRaw {
                        stat: PidStat {
                            comm: format!("p{pid}"),
                            state: if ticks > 0 { 'R' } else { 'S' },
                            ticks,
                            start: 1,
                        },
                        status: PidStatus { uid: 1000, rss },
                    };
                    (pid, p)
                })
                .collect(),
        }
    }

    fn host() -> Host {
        Host {
            hostname: None,
            mem: MemInfo {
                total: 1000,
                used: 500,
                swap_total: 0,
                swap_used: 0,
            },
            load: LoadAvg::default(),
            uptime: 0,
        }
    }

    #[test]
    fn process_cpu_is_relative_to_one_core() {
        // 2 cores, 200 ticks elapsed in total = 100 ticks of wall time.
        let prev = raw(CpuTimes { busy: 0, total: 0 }, &[(1, 0, 10), (2, 0, 900)]);
        let now = raw(
            CpuTimes {
                busy: 150,
                total: 200,
            },
            &[(1, 100, 10), (2, 50, 900), (3, 999, 1)],
        );
        let users = HashMap::from([(1000, "me".to_owned())]);
        let s = build(&prev, &now, host(), &users, |pid| format!("cmd{pid}"));

        assert!((s.cpu - 75.0).abs() < 1e-9);
        assert_eq!(s.cpus, [75.0, 75.0]);
        assert_eq!(s.tasks, 3);
        let p1 = s.procs.iter().find(|p| p.pid == 1).unwrap();
        assert!((p1.cpu - 100.0).abs() < 1e-9, "a full core");
        assert_eq!(p1.user.as_deref(), Some("me"));
        let p2 = s.procs.iter().find(|p| p.pid == 2).unwrap();
        assert!((p2.cpu - 50.0).abs() < 1e-9);
        assert!((p2.mem - 90.0).abs() < 1e-9);
        let p3 = s.procs.iter().find(|p| p.pid == 3).unwrap();
        assert!(
            p3.cpu.abs() < f64::EPSILON,
            "new process has no baseline yet"
        );
        assert_eq!(s.procs[0].pid, 1, "highest CPU first");
    }

    #[test]
    fn reused_pid_is_not_compared_with_the_old_process() {
        let prev = raw(CpuTimes::default(), &[(1, 5000, 1)]);
        let mut now = raw(
            CpuTimes {
                busy: 10,
                total: 200,
            },
            &[(1, 40, 1)],
        );
        now.procs.get_mut(&1).unwrap().stat.start = 2;
        let s = build(&prev, &now, host(), &HashMap::new(), |_| String::new());
        assert!(s.procs[0].cpu.abs() < f64::EPSILON);
        assert_eq!(
            s.procs[0].cmd, "[p1]",
            "kernel-thread style name without cmdline"
        );
        assert_eq!(s.procs[0].user, None);
    }

    #[test]
    fn top_lists_are_bounded_and_cover_both_sort_keys() {
        // 15 busy small processes and 15 idle big ones.
        let mut procs: Vec<(u32, u64, u64)> = (1..=15).map(|p| (p, u64::from(p), 1)).collect();
        procs.extend((100..115).map(|p| (p, 0, u64::from(p) * 10)));
        let prev = raw(
            CpuTimes::default(),
            &procs.iter().map(|&(p, _, r)| (p, 0, r)).collect::<Vec<_>>(),
        );
        let now = raw(
            CpuTimes {
                busy: 100,
                total: 200,
            },
            &procs,
        );
        let s = build(&prev, &now, host(), &HashMap::new(), |_| "x".to_owned());
        assert_eq!(s.procs.len(), 2 * TOP);
        assert_eq!(s.procs[0].pid, 15);
        assert!(
            s.procs.iter().any(|p| p.pid == 114),
            "biggest by memory included"
        );
        assert!(
            !s.procs.iter().any(|p| p.pid == 1),
            "neither busy nor big enough"
        );
    }

    #[tokio::test]
    async fn one_off_snapshot_from_a_fake_proc_dir() {
        let fake = FakeProc::new();
        let mon = SystemMonitor::new(
            fake.0.clone(),
            Duration::from_millis(50),
            CancellationToken::new(),
        );
        let s = mon.snapshot().await.unwrap();
        assert_eq!(s.hostname.as_deref(), Some("box"));
        assert_eq!(
            s.mem,
            Usage {
                total: 1_024_000,
                used: 768_000
            }
        );
        assert_eq!(s.uptime_secs, 3600);
        assert_eq!((s.tasks, s.running, s.threads), (2, 1, 40));
        assert_eq!(s.cpus.len(), 2);
        let init = s.procs.iter().find(|p| p.pid == 1).unwrap();
        assert_eq!(init.cmd, "/sbin/init");
        assert!((init.mem - 10.0).abs() < 1e-9);
        assert!(s.procs.iter().any(|p| p.cmd == "[kthreadd]"));
        assert!(
            !mon.is_sampling(),
            "a one-off sample starts no background task"
        );
    }

    #[tokio::test]
    async fn unreadable_pids_are_skipped() {
        let fake = FakeProc::new();
        // Listed but its files are gone (the process exited mid-scan).
        std::fs::create_dir_all(fake.0.join("77")).unwrap();
        fake.write("88/stat", "garbage");
        let mon = SystemMonitor::new(
            fake.0.clone(),
            Duration::from_millis(50),
            CancellationToken::new(),
        );
        assert_eq!(mon.snapshot().await.unwrap().tasks, 2);
    }

    #[tokio::test]
    async fn missing_proc_dir_is_an_error() {
        let mon = SystemMonitor::new(
            PathBuf::from("/nonexistent/cuthulu"),
            Duration::from_millis(50),
            CancellationToken::new(),
        );
        let e = mon.snapshot().await.unwrap_err();
        assert!(e.to_string().contains("/nonexistent/cuthulu/stat"), "{e}");
    }

    #[tokio::test]
    async fn samples_only_while_subscribed() {
        let fake = FakeProc::new();
        let mon = SystemMonitor::new(
            fake.0.clone(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );
        assert!(!mon.is_sampling());

        let mut a = mon.subscribe();
        let b = mon.subscribe();
        assert!(mon.is_sampling());
        a.recv().await.unwrap().unwrap();
        fake.cpu(50, 50);
        tokio::time::timeout(Duration::from_secs(5), async {
            while a.recv().await.unwrap().unwrap().cpu <= 0.0 {}
        })
        .await
        .expect("CPU% from the delta between samples");

        drop((a, b));
        tokio::time::timeout(Duration::from_secs(5), async {
            while mon.is_sampling() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sampler stops without subscribers");

        // And starts again for the next one.
        let mut c = mon.subscribe();
        assert!(mon.is_sampling());
        c.recv().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sampler_stops_on_shutdown() {
        let fake = FakeProc::new();
        let token = CancellationToken::new();
        let mon = SystemMonitor::new(fake.0.clone(), Duration::from_secs(3600), token.clone());
        let mut rx = mon.subscribe();
        rx.recv().await.unwrap().unwrap();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while mon.is_sampling() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("sampler stops on shutdown");
    }
}
