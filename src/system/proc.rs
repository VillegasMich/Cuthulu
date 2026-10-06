//! Pure parsers for the `/proc` files the host panel reads.
//!
//! Every function takes the file's contents and returns plain data, so all
//! of them are unit-tested with fixture strings and none touches the disk.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

/// A `/proc` file did not have the expected shape.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot parse {file}: {reason}")]
pub struct ParseError {
    pub file: &'static str,
    pub reason: String,
}

fn err(file: &'static str, reason: impl Into<String>) -> ParseError {
    ParseError {
        file,
        reason: reason.into(),
    }
}

/// Cumulative CPU time of one `cpu` line, in clock ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// Everything except idle and iowait.
    pub busy: u64,
    /// Sum of user, nice, system, idle, iowait, irq, softirq and steal.
    /// Guest time is already counted in user, so it is left out.
    pub total: u64,
}

impl CpuTimes {
    /// Busy share of the time elapsed between `prev` and `self`, in percent.
    #[must_use]
    pub fn percent_since(self, prev: Self) -> f64 {
        let total = self.total.saturating_sub(prev.total);
        if total == 0 {
            return 0.0;
        }
        let busy = self.busy.saturating_sub(prev.busy).min(total);
        ratio(busy, total) * 100.0
    }
}

/// `/proc/stat`: the aggregate line plus one entry per online core.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stat {
    pub all: CpuTimes,
    /// `(core id, times)` in file order. Offline cores have no line.
    pub cores: Vec<(u32, CpuTimes)>,
}

pub fn parse_stat(s: &str) -> Result<Stat, ParseError> {
    const FILE: &str = "stat";
    let mut all = None;
    let mut cores = Vec::new();
    for line in s.lines() {
        let mut it = line.split_ascii_whitespace();
        let Some(label) = it.next() else { continue };
        let Some(rest) = label.strip_prefix("cpu") else {
            continue;
        };
        let fields = it
            .take(8)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| err(FILE, format!("{label}: {e}")))?;
        if fields.len() < 4 {
            return Err(err(FILE, format!("{label}: too few fields")));
        }
        let total = fields.iter().sum();
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        let times = CpuTimes {
            busy: total - idle,
            total,
        };
        if rest.is_empty() {
            all = Some(times);
        } else {
            let id = rest
                .parse()
                .map_err(|_| err(FILE, format!("bad cpu label `{label}`")))?;
            cores.push((id, times));
        }
    }
    let all = all.ok_or_else(|| err(FILE, "no aggregate cpu line"))?;
    Ok(Stat { all, cores })
}

/// `/proc/meminfo`, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    /// Memory in use by processes, i.e. not free, buffers or reclaimable cache.
    pub used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

pub fn parse_meminfo(s: &str) -> Result<MemInfo, ParseError> {
    const FILE: &str = "meminfo";
    let mut kv: HashMap<&str, u64> = HashMap::new();
    for line in s.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // "MemTotal:       16303552 kB"; a few counters have no unit.
        let mut it = value.split_ascii_whitespace();
        let Some(Ok(n)) = it.next().map(str::parse::<u64>) else {
            continue;
        };
        let bytes = if it.next() == Some("kB") { n * 1024 } else { n };
        kv.insert(key.trim(), bytes);
    }
    let get = |k: &str| kv.get(k).copied();
    let total = get("MemTotal").ok_or_else(|| err(FILE, "no MemTotal"))?;
    let used = if let Some(avail) = get("MemAvailable") {
        total.saturating_sub(avail)
    } else {
        // Kernels before 3.14: the same estimate htop makes.
        let cache = (get("Cached").unwrap_or(0) + get("SReclaimable").unwrap_or(0))
            .saturating_sub(get("Shmem").unwrap_or(0));
        total
            .saturating_sub(get("MemFree").unwrap_or(0))
            .saturating_sub(get("Buffers").unwrap_or(0))
            .saturating_sub(cache)
    };
    let swap_total = get("SwapTotal").unwrap_or(0);
    let swap_used = swap_total.saturating_sub(get("SwapFree").unwrap_or(0));
    Ok(MemInfo {
        total,
        used,
        swap_total,
        swap_used,
    })
}

/// `/proc/loadavg`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
    /// Kernel scheduling entities (threads) runnable right now.
    pub running: u32,
    /// Kernel scheduling entities (threads) that exist right now.
    pub threads: u32,
}

pub fn parse_loadavg(s: &str) -> Result<LoadAvg, ParseError> {
    const FILE: &str = "loadavg";
    // "0.52 0.58 0.59 2/1234 5678"
    let mut it = s.split_ascii_whitespace();
    let mut load = || {
        it.next()
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| err(FILE, "expected three load averages"))
    };
    let (one, five, fifteen) = (load()?, load()?, load()?);
    let (running, threads) = it
        .next()
        .and_then(|v| v.split_once('/'))
        .and_then(|(r, n)| Some((r.parse().ok()?, n.parse().ok()?)))
        .ok_or_else(|| err(FILE, "expected running/total"))?;
    Ok(LoadAvg {
        one,
        five,
        fifteen,
        running,
        threads,
    })
}

/// `/proc/uptime`: whole seconds since boot.
pub fn parse_uptime(s: &str) -> Result<u64, ParseError> {
    s.split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        // Truncation is the point: uptime is shown in whole seconds at best.
        .map(|v| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let secs = v as u64;
            secs
        })
        .ok_or_else(|| err("uptime", "expected seconds"))
}

/// The fields of `/proc/[pid]/stat` the panel needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidStat {
    pub comm: String,
    pub state: char,
    /// utime + stime, in clock ticks.
    pub ticks: u64,
    /// Start time after boot, in clock ticks. Together with the pid it
    /// identifies a process across samples even when pids are reused.
    pub start: u64,
}

pub fn parse_pid_stat(s: &str) -> Result<PidStat, ParseError> {
    const FILE: &str = "pid/stat";
    // "1234 (some (odd) name) S 1 …": comm may hold spaces and parentheses,
    // so it ends at the *last* ')'.
    let open = s.find('(').ok_or_else(|| err(FILE, "no comm"))?;
    let close = s.rfind(')').ok_or_else(|| err(FILE, "no comm"))?;
    if close < open {
        return Err(err(FILE, "no comm"));
    }
    let comm = s[open + 1..close].to_owned();
    // Fields after comm, numbered from 3 (state) as in proc(5).
    let rest: Vec<&str> = s[close + 1..].split_ascii_whitespace().collect();
    let field = |n: usize| -> Result<u64, ParseError> {
        rest.get(n - 3)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| err(FILE, format!("field {n}")))
    };
    let state = rest
        .first()
        .and_then(|v| v.chars().next())
        .ok_or_else(|| err(FILE, "no state"))?;
    Ok(PidStat {
        comm,
        state,
        ticks: field(14)? + field(15)?,
        start: field(22)?,
    })
}

/// The fields of `/proc/[pid]/status` the panel needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PidStatus {
    /// Real uid.
    pub uid: u32,
    /// Resident set size in bytes; 0 for kernel threads.
    pub rss: u64,
}

pub fn parse_pid_status(s: &str) -> Result<PidStatus, ParseError> {
    const FILE: &str = "pid/status";
    let mut uid = None;
    let mut rss = 0;
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("Uid:") {
            uid = v
                .split_ascii_whitespace()
                .next()
                .and_then(|u| u.parse().ok());
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v
                .split_ascii_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .map_or(0, |kb| kb * 1024);
        }
    }
    Ok(PidStatus {
        uid: uid.ok_or_else(|| err(FILE, "no Uid"))?,
        rss,
    })
}

/// `/proc/[pid]/cmdline`: NUL-separated argv joined with spaces, at most
/// `max` bytes (cut on a char boundary). Empty for kernel threads.
#[must_use]
pub fn parse_cmdline(raw: &[u8], max: usize) -> String {
    let raw = raw.strip_suffix(b"\0").unwrap_or(raw);
    let mut s: String = String::from_utf8_lossy(raw)
        .chars()
        .map(|c| if c == '\0' || c.is_control() { ' ' } else { c })
        .collect();
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s.trim_end().to_owned()
}

/// `/etc/passwd`: uid → user name.
#[must_use]
pub fn parse_passwd(s: &str) -> HashMap<u32, String> {
    s.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split(':');
            let name = it.next()?;
            let uid = it.nth(1)?.parse().ok()?;
            (!name.is_empty()).then(|| (uid, name.to_owned()))
        })
        .collect()
}

/// `/proc/net/dev`: interface → (received bytes, transmitted bytes).
#[must_use]
pub fn parse_net_dev(s: &str) -> HashMap<String, (u64, u64)> {
    s.lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let f: Vec<u64> = rest
                .split_ascii_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            // 8 receive counters, then transmit; bytes come first in each.
            Some((name.trim().to_owned(), (*f.first()?, *f.get(8)?)))
        })
        .collect()
}

/// One IPv4 route from `/proc/net/route` (the main table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub iface: String,
    pub dest: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub metric: u32,
}

impl Route {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.dest.is_unspecified() && self.mask.is_unspecified()
    }

    #[must_use]
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & u32::from(self.mask) == u32::from(self.dest)
    }
}

pub fn parse_route(s: &str) -> Result<Vec<Route>, ParseError> {
    const FILE: &str = "net/route";
    // Addresses are the raw network-order bytes printed as a host-endian
    // hex number, so they read back with `to_le_bytes` on the (little-endian)
    // platforms this runs on.
    let addr = |h: &str| {
        u32::from_str_radix(h, 16)
            .map(|v| Ipv4Addr::from(v.to_le_bytes()))
            .map_err(|e| err(FILE, format!("`{h}`: {e}")))
    };
    s.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            if f.len() < 8 {
                return Err(err(FILE, "too few fields"));
            }
            Ok(Route {
                iface: f[0].to_owned(),
                dest: addr(f[1])?,
                mask: addr(f[7])?,
                metric: f[6].parse().map_err(|_| err(FILE, "bad metric"))?,
            })
        })
        .collect()
}

/// The host's own IPv4 addresses: the `/32 host LOCAL` leaves of
/// `/proc/net/fib_trie`, in order, without duplicates.
#[must_use]
pub fn parse_fib_trie_local(s: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut leaf = None;
    for line in s.lines() {
        let t = line.trim();
        if let Some(ip) = t.strip_prefix("|-- ") {
            leaf = ip.parse::<Ipv4Addr>().ok();
        } else if t.starts_with("+--") {
            leaf = None;
        } else if t == "/32 host LOCAL"
            && let Some(ip) = leaf.filter(|ip| !out.contains(ip))
        {
            out.push(ip);
        }
    }
    out
}

/// Global, non-temporary IPv6 addresses from `/proc/net/if_inet6`.
#[must_use]
pub fn parse_if_inet6(s: &str) -> Vec<(String, Ipv6Addr)> {
    const SCOPE_GLOBAL: u8 = 0;
    const FLAG_TEMPORARY: u32 = 0x01;
    s.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            let [addr, _, _, scope, flags, name] = f[..] else {
                return None;
            };
            let addr = Ipv6Addr::from(u128::from_str_radix(addr, 16).ok()?);
            let scope = u8::from_str_radix(scope, 16).ok()?;
            let flags = u32::from_str_radix(flags, 16).ok()?;
            (scope == SCOPE_GLOBAL && flags & FLAG_TEMPORARY == 0).then(|| (name.to_owned(), addr))
        })
        .collect()
}

/// `/proc/diskstats`: bytes (read, written) summed over whole physical
/// disks. Partitions, loop/ram/optical devices and device-mapper/md layers
/// are left out so nothing is counted twice.
#[must_use]
pub fn parse_diskstats(s: &str) -> (u64, u64) {
    // diskstats always counts 512-byte sectors, whatever the device uses.
    const SECTOR: u64 = 512;
    s.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            let name = *f.get(2)?;
            if !is_whole_disk(name) {
                return None;
            }
            let read: u64 = f.get(5)?.parse().ok()?;
            let written: u64 = f.get(9)?.parse().ok()?;
            Some((read * SECTOR, written * SECTOR))
        })
        .fold((0, 0), |(r, w), (dr, dw)| (r + dr, w + dw))
}

fn is_whole_disk(name: &str) -> bool {
    const VIRTUAL: [&str; 7] = ["loop", "ram", "zram", "sr", "fd", "dm-", "md"];
    if VIRTUAL.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.len() == name.len() {
        return true; // sda, vdb
    }
    if ["sd", "vd", "xvd", "hd"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        return false; // sda1
    }
    // nvme0n1p2, mmcblk0p1: "<disk ending in a digit>p<n>" is a partition.
    !stem
        .strip_suffix('p')
        .is_some_and(|d| d.ends_with(|c: char| c.is_ascii_digit()))
}

/// `a / b` for counters that comfortably fit an `f64` mantissa.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "\
cpu  596133 2025 180313 3812506 8980 0 4963 0 0 0
cpu0 76777 237 23894 471023 1195 0 88 0 0 0
cpu2 75093 196 24054 473162 1414 0 136 0 0 0
intr 32060643 152 6 0 0
ctxt 123
procs_running 3
";

    #[test]
    fn stat_aggregate_and_cores() {
        let s = parse_stat(STAT).unwrap();
        let total = 596_133 + 2025 + 180_313 + 3_812_506 + 8980 + 4963;
        assert_eq!(s.all.total, total);
        assert_eq!(s.all.busy, total - 3_812_506 - 8980);
        assert_eq!(
            s.cores.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [0, 2],
            "ids come from the label, offline cores are skipped"
        );
    }

    #[test]
    fn stat_old_kernel_with_four_fields() {
        let s = parse_stat("cpu 10 0 10 80\n").unwrap();
        assert_eq!((s.all.busy, s.all.total), (20, 100));
        assert_eq!(s.cores, []);
    }

    #[test]
    fn stat_rejects_garbage() {
        assert!(parse_stat("").is_err());
        assert!(parse_stat("cpu 1 x 3 4\n").is_err());
        assert!(parse_stat("cpu 1 2\n").is_err());
    }

    #[test]
    fn cpu_percent_from_deltas() {
        let a = CpuTimes {
            busy: 100,
            total: 1000,
        };
        let b = CpuTimes {
            busy: 150,
            total: 1200,
        };
        assert!((b.percent_since(a) - 25.0).abs() < 1e-9);
        assert!(a.percent_since(a).abs() < f64::EPSILON, "no time passed");
        assert!(a.percent_since(b).abs() < f64::EPSILON, "counter reset");
    }

    const MEMINFO: &str = "\
MemTotal:       16303552 kB
MemFree:         1203320 kB
MemAvailable:   10214788 kB
Buffers:          513276 kB
Cached:          8001020 kB
SwapCached:            0 kB
SwapTotal:       2097148 kB
SwapFree:        1997148 kB
HugePages_Total:       0
Shmem:            600000 kB
SReclaimable:     400000 kB
";

    #[test]
    fn meminfo_uses_mem_available() {
        let m = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(m.total, 16_303_552 * 1024);
        assert_eq!(m.used, (16_303_552 - 10_214_788) * 1024);
        assert_eq!(m.swap_total, 2_097_148 * 1024);
        assert_eq!(m.swap_used, 100_000 * 1024);
    }

    #[test]
    fn meminfo_without_mem_available() {
        let old = MEMINFO.replace("MemAvailable", "Ignored");
        let m = parse_meminfo(&old).unwrap();
        let kb = 16_303_552 - 1_203_320 - 513_276 - (8_001_020 + 400_000 - 600_000);
        assert_eq!(m.used, kb * 1024);
    }

    #[test]
    fn meminfo_needs_total_and_tolerates_no_swap() {
        assert!(parse_meminfo("MemFree: 1 kB\n").is_err());
        let m = parse_meminfo("MemTotal: 4 kB\nMemAvailable: 1 kB\n").unwrap();
        assert_eq!((m.used, m.swap_total, m.swap_used), (3 * 1024, 0, 0));
    }

    #[test]
    fn loadavg() {
        let l = parse_loadavg("8.47 3.08 1.80 21/1708 68864\n").unwrap();
        assert!((l.one - 8.47).abs() < 1e-9);
        assert!((l.fifteen - 1.80).abs() < 1e-9);
        assert_eq!((l.running, l.threads), (21, 1708));
        assert!(parse_loadavg("1.0 2.0\n").is_err());
        assert!(parse_loadavg("1.0 2.0 3.0 nope\n").is_err());
    }

    #[test]
    fn uptime() {
        assert_eq!(parse_uptime("5820.69 38125.06\n").unwrap(), 5820);
        assert!(parse_uptime("").is_err());
        assert!(parse_uptime("-1 0").is_err());
    }

    #[test]
    fn pid_stat_with_tricky_comm() {
        // utime=17 (field 14) stime=3 (field 15) starttime=4242 (field 22)
        let line = "77 (tmux: server) (x)) R 1 77 77 0 -1 4194560 100 0 0 0 17 3 0 0 20 0 1 0 4242 1000 200 18446744073709551615";
        let p = parse_pid_stat(line).unwrap();
        assert_eq!(p.comm, "tmux: server) (x)");
        assert_eq!(p.state, 'R');
        assert_eq!(p.ticks, 20);
        assert_eq!(p.start, 4242);
    }

    #[test]
    fn pid_stat_rejects_truncated() {
        assert!(parse_pid_stat("").is_err());
        assert!(parse_pid_stat("1 (x) S 1 2 3").is_err());
        assert!(parse_pid_stat("1 )x( S").is_err());
    }

    #[test]
    fn pid_status() {
        let s = "Name:\tbash\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t    2020 kB\n";
        assert_eq!(
            parse_pid_status(s).unwrap(),
            PidStatus {
                uid: 1000,
                rss: 2020 * 1024
            }
        );
        // Kernel threads have no VmRSS line.
        let k = parse_pid_status("Name:\tkthreadd\nUid:\t0\t0\t0\t0\n").unwrap();
        assert_eq!(k.rss, 0);
        assert!(parse_pid_status("Name:\tx\n").is_err());
    }

    #[test]
    fn cmdline() {
        assert_eq!(
            parse_cmdline(b"/usr/bin/python3\0-m\0http.server\0", 100),
            "/usr/bin/python3 -m http.server"
        );
        assert_eq!(parse_cmdline(b"", 100), "");
        assert_eq!(parse_cmdline(b"abc\x1b[31m\0", 100), "abc [31m");
        assert_eq!(
            parse_cmdline("aé".as_bytes(), 2),
            "a",
            "cut on a char boundary"
        );
    }

    #[test]
    fn passwd() {
        let users = parse_passwd(
            "# comment\nroot:x:0:0:root:/root:/bin/bash\nmanuel:x:1000:1000::/home/m:/bin/zsh\nbroken\n",
        );
        assert_eq!(users.len(), 2);
        assert_eq!(users[&1000], "manuel");
    }

    #[test]
    fn net_dev() {
        let s = "Inter-|   Receive |  Transmit\n face |bytes packets|bytes\n    lo: 155 30 0 0 0 0 0 0 155 30 0 0 0 0 0 0\nwlp2s0: 1265857872 1123552 0 249 0 0 0 0 158820816 322206 0 0 0 0 0 0\n";
        let m = parse_net_dev(s);
        assert_eq!(m.len(), 2);
        assert_eq!(m["wlp2s0"], (1_265_857_872, 158_820_816));
    }

    const ROUTE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
wlp2s0\t00000000\tFE01A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
wlp2s0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0\n";

    #[test]
    fn routes() {
        let r = parse_route(ROUTE).unwrap();
        assert_eq!(r.len(), 3);
        assert!(r[0].is_default());
        assert_eq!(r[0].metric, 600);
        assert_eq!(r[1].dest, Ipv4Addr::new(172, 17, 0, 0));
        assert_eq!(r[1].mask, Ipv4Addr::new(255, 255, 0, 0));
        assert!(r[2].contains(Ipv4Addr::new(192, 168, 1, 57)));
        assert!(!r[2].contains(Ipv4Addr::new(192, 168, 2, 1)));
        assert!(parse_route("h\nx 0 0 0\n").is_err());
    }

    #[test]
    fn fib_trie_local_addresses() {
        let s = "\
Main:
  +-- 0.0.0.0/0 3 0 5
     |-- 192.168.1.0
        /24 link UNICAST
Local:
  +-- 0.0.0.0/0 3 1 5
     +-- 127.0.0.0/8 2 0 2
        |-- 127.0.0.0
           /8 host LOCAL
        |-- 127.0.0.1
           /32 host LOCAL
     +-- 192.168.1.0/24 2 0 2
        |-- 192.168.1.57
           /32 host LOCAL
        |-- 192.168.1.255
           /32 link BROADCAST
  +-- 10.0.0.0/8 2 0 2
     |-- 192.168.1.57
        /32 host LOCAL
";
        assert_eq!(
            parse_fib_trie_local(s),
            [Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 1, 57)]
        );
    }

    #[test]
    fn if_inet6_global_only() {
        let s = "\
fe80000000000000d5df9e8b71121a39 02 40 20 80   wlp2s0
2a0102030405060700000000000000aa 02 40 00 80   wlp2s0
2a0102030405060700000000000000bb 02 40 00 01   wlp2s0
00000000000000000000000000000001 01 80 10 80       lo
";
        let v = parse_if_inet6(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "wlp2s0");
        assert_eq!(v[0].1.to_string(), "2a01:203:405:607::aa");
    }

    #[test]
    fn diskstats_whole_disks_only() {
        let s = "\
   7       0 loop0 14 0 34 1 0 0 0 0 0 1 1 0 0 0 0 0 0
 259       0 nvme0n1 100 0 1000 0 50 0 2000 0 0 0 0
 259       1 nvme0n1p1 10 0 100 0 5 0 200 0 0 0 0
   8       0 sda 1 0 10 0 1 0 20 0 0 0 0
   8       1 sda1 1 0 10 0 1 0 20 0 0 0 0
 179       0 mmcblk0 1 0 4 0 1 0 8 0 0 0 0
 179       1 mmcblk0p1 1 0 4 0 1 0 8 0 0 0 0
 253       0 dm-0 1 0 999 0 1 0 999 0 0 0 0
";
        assert_eq!(
            parse_diskstats(s),
            ((1000 + 10 + 4) * 512, (2000 + 20 + 8) * 512)
        );
    }
}
