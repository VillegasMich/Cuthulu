//! Host system stats read from `/proc`: CPU, memory, load, network,
//! disk I/O and (opt-in) the busiest processes.
//!
//! This is not a [`Provider`](crate::providers::Provider): the host is not a
//! service source. One shared sampler runs only while at least one client is
//! subscribed and stops on its own when the last one leaves, so nobody
//! watching costs nothing. Results go out on a small bounded broadcast;
//! every update is a full snapshot, so a lagging subscriber just skips some.

pub mod proc;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use self::proc::{LoadAvg, MemInfo, PidStat, PidStatus, Route, Stat, ratio};
use crate::config::Config;

/// Processes listed per sort key. The snapshot carries the union of the top
/// `TOP` by CPU and by memory, so the client can sort by either.
pub const TOP: usize = 10;
/// Longest command line sent per process, in bytes.
const MAX_CMD: usize = 512;
/// Addresses sent per snapshot.
const MAX_ADDRS: usize = 12;
/// Snapshots buffered per subscriber; older ones are skipped, never queued.
const CAPACITY: usize = 4;
/// Gap between the two reads of a one-off sample (rates need a delta).
const BASELINE: Duration = Duration::from_millis(250);
const PASSWD: &str = "/etc/passwd";
/// Interfaces that are container or VM plumbing, not the host's own links.
const VIRTUAL_IFACES: [&str; 9] = [
    "lo", "docker", "br-", "veth", "virbr", "cni", "flannel", "cali", "vxlan",
];

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
    /// `None` when the network files cannot be read.
    pub net: Option<Net>,
    /// `None` when `/proc/diskstats` cannot be read.
    pub disk: Option<DiskIo>,
    /// Only with `CUTHULU_SYSTEM_PROCESSES`: the union of the top
    /// [`TOP`] by CPU and by memory, highest CPU first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub procs: Option<Vec<Process>>,
}

/// Bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub total: u64,
    pub used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Net {
    /// Interface of the default route.
    pub iface: Option<String>,
    /// The host's addresses, those on `iface` first. Loopback and
    /// container bridges are left out.
    pub addrs: Vec<Addr>,
    /// Bytes per second received / sent on `iface`, or summed over the
    /// non-virtual interfaces when there is no default route.
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Addr {
    pub ip: String,
    /// `None` when no main-table route says (e.g. a VPN with its own table).
    pub iface: Option<String>,
    /// On the default-route interface.
    pub primary: bool,
    pub kind: AddrKind,
}

/// What an address is for, shown as a label next to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AddrKind {
    /// Private LAN range (RFC 1918, link-local, IPv6 ULA).
    Local,
    /// Globally routable.
    Public,
    /// Carrier-grade NAT (100.64.0.0/10) handed out by the ISP.
    Cgnat,
    Tailscale,
    Wireguard,
    Zerotier,
    /// Some other tunnel (`tun*`, `tap*`).
    Vpn,
}

/// Bytes per second read from / written to the physical disks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskIo {
    pub read: u64,
    pub write: u64,
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

/// Every counter a delta needs, from one point in time.
#[derive(Debug, Clone)]
struct Raw {
    at: Instant,
    stat: Stat,
    tasks: u32,
    /// Empty unless processes are enabled.
    procs: HashMap<u32, ProcRaw>,
    /// Interface → (rx, tx) bytes.
    net: Option<HashMap<String, (u64, u64)>>,
    /// (read, written) bytes.
    disk: Option<(u64, u64)>,
}

#[derive(Default)]
struct State {
    running: bool,
    latest: Option<(Instant, Update)>,
}

pub struct SystemMonitor {
    proc_dir: PathBuf,
    interval: Duration,
    processes: bool,
    users: HashMap<u32, String>,
    /// `sys/kernel/hostname` answers for the *reader's* UTS namespace, so
    /// inside a container it is the container's name, not the host's.
    read_hostname: bool,
    tx: broadcast::Sender<Update>,
    state: Mutex<State>,
    /// Previous raw read, the baseline for the next delta.
    prev: Mutex<Option<Raw>>,
    shutdown: CancellationToken,
}

impl SystemMonitor {
    #[must_use]
    pub fn new(config: &Config, shutdown: CancellationToken) -> Arc<Self> {
        let processes = config.system_processes;
        // Optional: in the container the host's file is mounted read-only;
        // without it users are shown by uid.
        let users = if processes {
            std::fs::read_to_string(PASSWD)
                .map(|s| proc::parse_passwd(&s))
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        Arc::new(Self {
            proc_dir: config.proc_dir.clone(),
            interval: config.system_interval,
            processes,
            users,
            read_hostname: !Path::new("/.dockerenv").exists(),
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
            .read_hostname
            .then(|| self.read("sys/kernel/hostname").ok())
            .flatten()
            .map(|h| h.trim().to_owned())
            .filter(|h| !h.is_empty());

        let host = Host {
            hostname,
            mem,
            load,
            uptime,
            addrs: self.addresses(),
        };
        let mut snap = build(&base, &now, host);
        if self.processes {
            snap.procs = Some(top_processes(&base, &now, mem.total, &self.users, |pid| {
                std::fs::read(self.proc_dir.join(pid.to_string()).join("cmdline"))
                    .map(|raw| proc::parse_cmdline(&raw, MAX_CMD))
                    .unwrap_or_default()
            }));
        }
        *prev = Some(now);
        Ok(snap)
    }

    fn read(&self, rel: &str) -> Result<String, SystemError> {
        let path = self.proc_dir.join(rel);
        std::fs::read_to_string(&path).map_err(|e| io_err(&path, &e))
    }

    /// A file under `net/`. `<proc>/net` is a link to `self/net`: the
    /// *reader's* network namespace, which in a container is the
    /// container's. Pid 1's view is the host's, so prefer that.
    fn read_net(&self, file: &str) -> Option<String> {
        self.read(&format!("1/net/{file}"))
            .or_else(|_| self.read(&format!("net/{file}")))
            .ok()
    }

    fn addresses(&self) -> Option<Addrs> {
        let routes = proc::parse_route(&self.read_net("route")?).ok()?;
        let v4 = self
            .read_net("fib_trie")
            .map(|s| proc::parse_fib_trie_local(&s))
            .unwrap_or_default();
        let v6 = self
            .read_net("if_inet6")
            .map(|s| proc::parse_if_inet6(&s))
            .unwrap_or_default();
        Some(addresses(&routes, &v4, &v6))
    }

    fn read_raw(&self) -> Result<Raw, SystemError> {
        let at = Instant::now();
        let stat = proc::parse_stat(&self.read("stat")?)?;
        let dir = std::fs::read_dir(&self.proc_dir).map_err(|e| io_err(&self.proc_dir, &e))?;
        let mut tasks = 0u32;
        let mut procs = HashMap::new();
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            tasks += 1;
            // Processes vanish between listing and reading, and `hidepid`
            // hides other users' ones: skip anything unreadable.
            if self.processes
                && let Some(p) = read_proc(&entry.path())
            {
                procs.insert(pid, p);
            }
        }
        Ok(Raw {
            at,
            stat,
            tasks,
            procs,
            net: self.read_net("dev").map(|s| proc::parse_net_dev(&s)),
            disk: self
                .read("diskstats")
                .ok()
                .map(|s| proc::parse_diskstats(&s)),
        })
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

fn is_virtual(iface: &str) -> bool {
    VIRTUAL_IFACES.iter().any(|p| iface.starts_with(p))
}

/// Labels an address from its interface name, then its range.
fn classify(ip: IpAddr, iface: Option<&str>, primary: bool) -> AddrKind {
    let named = [
        ("tailscale", AddrKind::Tailscale),
        ("wg", AddrKind::Wireguard),
        ("zt", AddrKind::Zerotier),
        ("tun", AddrKind::Vpn),
        ("tap", AddrKind::Vpn),
    ];
    if let Some(kind) = iface.and_then(|i| named.iter().find(|(p, _)| i.starts_with(p))) {
        return kind.1;
    }
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if a == 100 && (64..128).contains(&b) {
                // The shared range: an ISP's CGNAT on the uplink, otherwise
                // almost always Tailscale (whose addresses come from it).
                if primary {
                    AddrKind::Cgnat
                } else {
                    AddrKind::Tailscale
                }
            } else if v4.is_private() || v4.is_link_local() {
                AddrKind::Local
            } else {
                AddrKind::Public
            }
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            if seg[..3] == [0xfd7a, 0x115c, 0xa1e0] {
                AddrKind::Tailscale
            } else if seg[0] & 0xfe00 == 0xfc00 || seg[0] & 0xffc0 == 0xfe80 {
                AddrKind::Local
            } else {
                AddrKind::Public
            }
        }
    }
}

/// The host's addresses and the interface of the default route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Addrs {
    iface: Option<String>,
    list: Vec<Addr>,
}

fn addresses(routes: &[Route], v4: &[Ipv4Addr], v6: &[(String, Ipv6Addr)]) -> Addrs {
    let iface = routes
        .iter()
        .filter(|r| r.is_default())
        .min_by_key(|r| r.metric)
        .map(|r| r.iface.clone());
    let mut list = Vec::new();
    let mut place = |on: Option<&str>, ip: IpAddr| {
        if on.is_some_and(is_virtual) {
            return;
        }
        let primary = on.is_some() && on == iface.as_deref();
        list.push(Addr {
            ip: ip.to_string(),
            iface: on.map(str::to_owned),
            primary,
            kind: classify(ip, on, primary),
        });
    };
    for ip in v4.iter().filter(|ip| !ip.is_loopback()) {
        // Longest matching prefix; masks are contiguous, so bigger is longer.
        let on = routes
            .iter()
            .filter(|r| !r.is_default() && r.contains(*ip))
            .max_by_key(|r| u32::from(r.mask))
            .map(|r| r.iface.as_str());
        place(on, IpAddr::V4(*ip));
    }
    for (i, ip) in v6 {
        place(Some(i), IpAddr::V6(*ip));
    }
    // Primary interface first, otherwise in discovery order (IPv4 first).
    list.sort_by_key(|a| !a.primary);
    list.truncate(MAX_ADDRS);
    Addrs { iface, list }
}

/// Host-wide values that need no delta.
struct Host {
    hostname: Option<String>,
    mem: MemInfo,
    load: LoadAvg,
    uptime: u64,
    addrs: Option<Addrs>,
}

/// Bytes per second between two counter readings.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn rate(before: u64, after: u64, secs: f64) -> u64 {
    if secs <= 0.0 {
        return 0;
    }
    (after.saturating_sub(before) as f64 / secs).round() as u64
}

/// (rx, tx) of `iface`, or summed over the non-virtual interfaces.
fn traffic(counters: &HashMap<String, (u64, u64)>, iface: Option<&str>) -> (u64, u64) {
    counters
        .iter()
        .filter(|(name, _)| iface.map_or(!is_virtual(name), |i| i == name.as_str()))
        .fold((0, 0), |(r, t), (_, (dr, dt))| (r + dr, t + dt))
}

/// Turns two raw reads into a snapshot (without processes).
fn build(prev: &Raw, now: &Raw, host: Host) -> Snapshot {
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

    let secs = now.at.saturating_duration_since(prev.at).as_secs_f64();
    let net = now.net.as_ref().map(|counters| {
        let addrs = host.addrs.unwrap_or_default();
        let iface = addrs.iface.as_deref();
        let after = traffic(counters, iface);
        let before = prev.net.as_ref().map_or(after, |c| traffic(c, iface));
        Net {
            rx: rate(before.0, after.0, secs),
            tx: rate(before.1, after.1, secs),
            iface: addrs.iface,
            addrs: addrs.list,
        }
    });
    let disk = now.disk.map(|after| {
        let before = prev.disk.unwrap_or(after);
        DiskIo {
            read: rate(before.0, after.0, secs),
            write: rate(before.1, after.1, secs),
        }
    });

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
        tasks: now.tasks,
        threads: host.load.threads,
        running: host.load.running,
        uptime_secs: host.uptime,
        net,
        disk,
        procs: None,
    }
}

/// The busiest processes between two reads. Pure apart from `cmdline`,
/// which is only called for the processes that make the top lists.
fn top_processes(
    prev: &Raw,
    now: &Raw,
    mem_total: u64,
    users: &HashMap<u32, String>,
    cmdline: impl Fn(u32) -> String,
) -> Vec<Process> {
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

    top.into_iter()
        .map(|i| {
            let (pid, p, cpu) = all[i];
            let cmd = cmdline(pid);
            Process {
                pid,
                uid: p.status.uid,
                user: users.get(&p.status.uid).cloned(),
                cpu: round1(cpu),
                mem: round1(ratio(p.status.rss, mem_total) * 100.0),
                rss: p.status.rss,
                cmd: if cmd.is_empty() {
                    format!("[{}]", p.stat.comm)
                } else {
                    cmd
                },
            }
        })
        .collect()
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
            fake.write(
                "1/net/route",
                "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                 eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\n\
                 eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\n\
                 docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\n",
            );
            fake.write(
                "1/net/fib_trie",
                "Local:\n  +-- 0.0.0.0/0 3 0 5\n\
                 |-- 127.0.0.1\n /32 host LOCAL\n\
                 |-- 172.17.0.1\n /32 host LOCAL\n\
                 |-- 192.168.1.57\n /32 host LOCAL\n\
                 |-- 100.64.0.5\n /32 host LOCAL\n",
            );
            fake.write(
                "1/net/if_inet6",
                "2a0102030405060700000000000000aa 02 40 00 80 eth0\n",
            );
            // What a container's own namespace would show: must be ignored.
            fake.write("net/dev", "h\nh\n  eth0: 9 0 0 0 0 0 0 0 9 0 0 0 0 0 0 0\n");
            fake.net(0, 0);
            fake.disk(0, 0);
            fake
        }

        /// Host-namespace counters: eth0 plus a docker bridge.
        pub(crate) fn net(&self, rx: u64, tx: u64) {
            self.write(
                "1/net/dev",
                format!(
                    "h\nh\n    lo: 5 0 0 0 0 0 0 0 5 0 0 0 0 0 0 0\n  eth0: {rx} 0 0 0 0 0 0 0 {tx} 0 0 0 0 0 0 0\ndocker0: 7 0 0 0 0 0 0 0 7 0 0 0 0 0 0 0\n"
                ),
            );
        }

        /// Sectors read / written on one disk (plus a partition of it).
        pub(crate) fn disk(&self, read: u64, written: u64) {
            self.write(
                "diskstats",
                format!(
                    "   8       0 sda 1 0 {read} 0 1 0 {written} 0 0 0 0\n   8       1 sda1 1 0 {read} 0 1 0 {written} 0 0 0 0\n"
                ),
            );
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

    /// A monitor over `dir`, sampling every `ms` milliseconds.
    pub(crate) fn monitor(
        dir: &Path,
        ms: u64,
        processes: bool,
        shutdown: CancellationToken,
    ) -> Arc<SystemMonitor> {
        let config = Config {
            proc_dir: dir.to_owned(),
            system_interval: Duration::from_millis(ms),
            system_processes: processes,
            ..Config::default()
        };
        SystemMonitor::new(&config, shutdown)
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
            tasks: u32::try_from(procs.len()).unwrap(),
            net: None,
            disk: None,
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
            addrs: None,
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
        let s = build(&prev, &now, host());
        assert!((s.cpu - 75.0).abs() < 1e-9);
        assert_eq!(s.cpus, [75.0, 75.0]);
        assert_eq!(s.tasks, 3);
        assert_eq!(s.procs, None);

        let procs = top_processes(&prev, &now, 1000, &users, |pid| format!("cmd{pid}"));
        let p1 = procs.iter().find(|p| p.pid == 1).unwrap();
        assert!((p1.cpu - 100.0).abs() < 1e-9, "a full core");
        assert_eq!(p1.user.as_deref(), Some("me"));
        let p2 = procs.iter().find(|p| p.pid == 2).unwrap();
        assert!((p2.cpu - 50.0).abs() < 1e-9);
        assert!((p2.mem - 90.0).abs() < 1e-9);
        let p3 = procs.iter().find(|p| p.pid == 3).unwrap();
        assert!(
            p3.cpu.abs() < f64::EPSILON,
            "new process has no baseline yet"
        );
        assert_eq!(procs[0].pid, 1, "highest CPU first");
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
        let procs = top_processes(&prev, &now, 1000, &HashMap::new(), |_| String::new());
        assert!(procs[0].cpu.abs() < f64::EPSILON);
        assert_eq!(
            procs[0].cmd, "[p1]",
            "kernel-thread style name without cmdline"
        );
        assert_eq!(procs[0].user, None);
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
        let procs = top_processes(&prev, &now, 1000, &HashMap::new(), |_| "x".to_owned());
        assert_eq!(procs.len(), 2 * TOP);
        assert_eq!(procs[0].pid, 15);
        assert!(
            procs.iter().any(|p| p.pid == 114),
            "biggest by memory included"
        );
        assert!(
            !procs.iter().any(|p| p.pid == 1),
            "neither busy nor big enough"
        );
    }

    #[test]
    fn network_and_disk_rates_over_elapsed_time() {
        let mut prev = raw(CpuTimes::default(), &[]);
        let mut now = raw(CpuTimes::default(), &[]);
        prev.at = now.at.checked_sub(Duration::from_secs(2)).unwrap();
        let counters = |eth: u64, veth: u64| {
            Some(HashMap::from([
                ("eth0".to_owned(), (eth, eth / 2)),
                ("veth1".to_owned(), (veth, veth)),
            ]))
        };
        prev.net = counters(1000, 0);
        now.net = counters(5000, 1_000_000);
        prev.disk = Some((0, 0));
        now.disk = Some((4096, 1024));

        let mut h = host();
        h.addrs = Some(Addrs {
            iface: Some("eth0".to_owned()),
            ..Addrs::default()
        });
        let s = build(&prev, &now, h);
        let net = s.net.unwrap();
        assert_eq!((net.rx, net.tx), (2000, 1000), "eth0 only, per second");
        assert_eq!(
            s.disk,
            Some(DiskIo {
                read: 2048,
                write: 512
            })
        );

        // No default route: every non-virtual interface counts, veth does not.
        let s = build(&prev, &now, host());
        assert_eq!(s.net.unwrap().rx, 2000);
        // First sample: no baseline, so no made-up rate.
        prev.net = None;
        prev.disk = None;
        let s = build(&prev, &now, host());
        assert_eq!((s.net.unwrap().rx, s.disk.unwrap().read), (0, 0));
    }

    #[test]
    fn addresses_grouped_by_default_route() {
        let routes = proc::parse_route(
            "h\n\
             wlan0\t00000000\t0101A8C0\t3\t0\t0\t600\t00000000\n\
             eth9\t00000000\t0101A8C0\t3\t0\t0\t100\t00000000\n\
             eth9\t0001A8C0\t00000000\t1\t0\t0\t100\t00FFFFFF\n\
             wlan0\t0000000A\t00000000\t1\t0\t0\t600\t000000FF\n\
             br-1\t000012AC\t00000000\t1\t0\t0\t0\t0000FFFF\n",
        )
        .unwrap();
        let v4: Vec<Ipv4Addr> = [
            "127.0.0.1",
            "192.168.1.57",
            "10.0.0.8",
            "172.18.0.1",
            "100.64.0.5",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        let v6 = vec![("eth9".to_owned(), "2a01::aa".parse().unwrap())];
        let a = addresses(&routes, &v4, &v6);
        assert_eq!(
            a.iface.as_deref(),
            Some("eth9"),
            "lowest metric default route"
        );
        let got: Vec<(&str, Option<&str>, bool, AddrKind)> = a
            .list
            .iter()
            .map(|a| (a.ip.as_str(), a.iface.as_deref(), a.primary, a.kind))
            .collect();
        assert_eq!(
            got,
            [
                ("192.168.1.57", Some("eth9"), true, AddrKind::Local),
                ("2a01::aa", Some("eth9"), true, AddrKind::Public),
                ("10.0.0.8", Some("wlan0"), false, AddrKind::Local),
                ("100.64.0.5", None, false, AddrKind::Tailscale),
            ],
            "primary first; bridges and loopback left out"
        );
    }

    #[test]
    fn address_kinds() {
        let k = |ip: &str, iface: Option<&str>, primary: bool| {
            classify(ip.parse().unwrap(), iface, primary)
        };
        assert_eq!(k("192.168.1.57", Some("wlp2s0"), true), AddrKind::Local);
        assert_eq!(k("10.1.2.3", Some("eth0"), true), AddrKind::Local);
        assert_eq!(k("172.20.0.4", Some("eth0"), true), AddrKind::Local);
        assert_eq!(k("169.254.1.1", Some("eth0"), true), AddrKind::Local);
        assert_eq!(k("81.2.69.160", Some("eth0"), true), AddrKind::Public);
        assert_eq!(k("100.115.90.103", None, false), AddrKind::Tailscale);
        assert_eq!(k("100.72.1.1", Some("ppp0"), true), AddrKind::Cgnat);
        assert_eq!(
            k("100.128.0.1", Some("eth0"), true),
            AddrKind::Public,
            "outside 100.64/10"
        );
        assert_eq!(k("10.8.0.2", Some("wg0"), false), AddrKind::Wireguard);
        assert_eq!(
            k("10.147.17.5", Some("ztabcdef"), false),
            AddrKind::Zerotier
        );
        assert_eq!(k("10.8.0.6", Some("tun0"), false), AddrKind::Vpn);
        assert_eq!(
            k("fd7a:115c:a1e0::43a:5a67", Some("tailscale0"), false),
            AddrKind::Tailscale
        );
        assert_eq!(
            k("fd7a:115c:a1e0::1", None, false),
            AddrKind::Tailscale,
            "by range alone"
        );
        assert_eq!(k("fd12:3456::1", Some("eth0"), true), AddrKind::Local);
        assert_eq!(
            k("2800:e2:400:2e8::1", Some("wlp2s0"), true),
            AddrKind::Public
        );
    }

    #[tokio::test]
    async fn one_off_snapshot_from_a_fake_proc_dir() {
        let fake = FakeProc::new();
        let mon = monitor(&fake.0, 50, true, CancellationToken::new());
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
        let net = s.net.as_ref().unwrap();
        assert_eq!(
            net.iface.as_deref(),
            Some("eth0"),
            "host namespace via pid 1"
        );
        let ips: Vec<(&str, AddrKind)> =
            net.addrs.iter().map(|a| (a.ip.as_str(), a.kind)).collect();
        assert_eq!(
            ips,
            [
                ("192.168.1.57", AddrKind::Local),
                ("2a01:203:405:607::aa", AddrKind::Public),
                ("100.64.0.5", AddrKind::Tailscale),
            ]
        );
        assert!(s.disk.is_some());
        let procs = s.procs.as_ref().unwrap();
        let init = procs.iter().find(|p| p.pid == 1).unwrap();
        assert_eq!(init.cmd, "/sbin/init");
        assert!((init.mem - 10.0).abs() < 1e-9);
        assert!(procs.iter().any(|p| p.cmd == "[kthreadd]"));
        assert!(
            !mon.is_sampling(),
            "a one-off sample starts no background task"
        );
    }

    #[tokio::test]
    async fn processes_are_opt_in() {
        let fake = FakeProc::new();
        let mon = monitor(&fake.0, 50, false, CancellationToken::new());
        let s = mon.snapshot().await.unwrap();
        assert_eq!(s.procs, None);
        assert_eq!(s.tasks, 2, "still counted from the pid directories");
        let json = serde_json::to_value(&*s).unwrap();
        assert!(json.get("procs").is_none(), "{json}");
    }

    #[tokio::test]
    async fn unreadable_pids_are_skipped() {
        let fake = FakeProc::new();
        // Listed but its files are gone (the process exited mid-scan).
        std::fs::create_dir_all(fake.0.join("77")).unwrap();
        fake.write("88/stat", "garbage");
        let mon = monitor(&fake.0, 50, true, CancellationToken::new());
        assert_eq!(
            mon.snapshot().await.unwrap().procs.as_ref().unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn missing_network_files_are_not_fatal() {
        let fake = FakeProc::new();
        std::fs::remove_dir_all(fake.0.join("1/net")).unwrap();
        std::fs::remove_file(fake.0.join("net/dev")).unwrap();
        std::fs::remove_file(fake.0.join("diskstats")).unwrap();
        let mon = monitor(&fake.0, 50, false, CancellationToken::new());
        let s = mon.snapshot().await.unwrap();
        assert_eq!((s.net.as_ref(), s.disk), (None, None));
    }

    #[tokio::test]
    async fn missing_proc_dir_is_an_error() {
        let mon = monitor(
            Path::new("/nonexistent/cuthulu"),
            50,
            false,
            CancellationToken::new(),
        );
        let e = mon.snapshot().await.unwrap_err();
        assert!(e.to_string().contains("/nonexistent/cuthulu/stat"), "{e}");
    }

    #[tokio::test]
    async fn samples_only_while_subscribed() {
        let fake = FakeProc::new();
        let mon = monitor(&fake.0, 30, false, CancellationToken::new());
        assert!(!mon.is_sampling());

        let mut a = mon.subscribe();
        let b = mon.subscribe();
        assert!(mon.is_sampling());
        a.recv().await.unwrap().unwrap();
        fake.cpu(50, 50);
        fake.net(1_000_000, 0);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let s = a.recv().await.unwrap().unwrap();
                if s.cpu > 0.0 && s.net.as_ref().unwrap().rx > 0 {
                    break;
                }
            }
        })
        .await
        .expect("CPU% and rates from the delta between samples");

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
        let mon = monitor(&fake.0, 3_600_000, false, token.clone());
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
