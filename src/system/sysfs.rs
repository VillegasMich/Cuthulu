//! Power and thermal readings from sysfs: hwmon temperatures, the power
//! source (`power_supply`), the GPU's load and clock, and where the RAPL
//! energy counters (`powercap`) are.
//!
//! The parsers are pure and take plain values; the readers around them only
//! list directories and read small files under a root passed in, so the
//! tests run against temp dirs. Everything here is optional: a missing or
//! unreadable file leaves its value out, it never fails a snapshot.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Temperatures sent per snapshot.
const MAX_TEMPS: usize = 32;
/// Directory entries looked at per listing (hwmon chips, supplies, sensors).
const MAX_ENTRIES: usize = 64;
/// A `crit` outside this range (°C) is a placeholder, not a limit.
const CRIT_RANGE: std::ops::RangeInclusive<f64> = 40.0..=150.0;
/// Readings outside this range (°C) are unplugged or broken sensors.
const TEMP_RANGE: std::ops::RangeInclusive<f64> = -40.0..=200.0;

/// One temperature sensor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Temp {
    /// Friendly name from the driver (`cpu`, `gpu`, `nvme`, `disk`, `acpi`,
    /// else the driver), numbered (`nvme0`, `nvme1`) when several chips
    /// share it.
    pub name: String,
    /// hwmon driver name, e.g. `k10temp`.
    pub driver: String,
    /// The sensor's own label, e.g. `Tctl`, `edge`, `Composite`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub celsius: f64,
    /// The chip's critical temperature for this sensor, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crit: Option<f64>,
}

/// GPU load and clock (amdgpu exposes them; others may not).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gpu {
    pub driver: String,
    /// Busy %, 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<u8>,
    /// Current shader clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mhz: Option<u32>,
    /// Package power as the driver reports it (not every GPU does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watts: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Mains (or USB-C) power.
    Ac,
    Battery,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Supply {
    pub source: Source,
    /// Present when the machine has a system battery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<Battery>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Battery {
    /// Charge, 0–100 %.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u8>,
    /// Lowercase kernel status: `charging`, `discharging`, `full`,
    /// `not charging`, `unknown`.
    pub status: String,
    /// Rate of charge or discharge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watts: Option<f64>,
}

// ── temperatures and GPU (hwmon) ───────────────────────────

/// Friendly name of an hwmon driver.
#[must_use]
pub fn friendly(driver: &str) -> &str {
    match driver {
        "k10temp" | "coretemp" | "zenpower" | "cpu_thermal" => "cpu",
        "amdgpu" | "nouveau" | "radeon" | "i915" | "xe" => "gpu",
        "nvme" => "nvme",
        "drivetemp" => "disk",
        "acpitz" => "acpi",
        other => other,
    }
}

/// One `tempN_*` group of a chip, as read: millidegrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTemp {
    pub label: Option<String>,
    pub input: i64,
    pub crit: Option<i64>,
}

/// The sensors of one chip worth showing. CPU drivers list every core next
/// to the package; only the package-level reading is kept (`Tctl`,
/// `Package id N`, `Tdie`, else the first).
#[must_use]
pub fn chip_temps(driver: &str, raw: Vec<RawTemp>) -> Vec<RawTemp> {
    let raw: Vec<RawTemp> = raw
        .into_iter()
        .filter(|t| TEMP_RANGE.contains(&milli(t.input)))
        .collect();
    if friendly(driver) != "cpu" {
        return raw;
    }
    let rank = |t: &RawTemp| match t.label.as_deref() {
        Some("Tctl") => 0,
        Some(l) if l.starts_with("Package id") => 1,
        Some("Tdie") => 2,
        _ => 3,
    };
    raw.into_iter().min_by_key(rank).into_iter().collect()
}

/// Turns chips (in hwmon order) into named temperatures: chips whose
/// friendly name is shared get numbered in order.
#[must_use]
pub fn name_temps(chips: &[(String, Vec<RawTemp>)]) -> Vec<Temp> {
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for (driver, temps) in chips {
        if !temps.is_empty() {
            *count.entry(friendly(driver)).or_default() += 1;
        }
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::new();
    for (driver, temps) in chips {
        if temps.is_empty() {
            continue;
        }
        let base = friendly(driver);
        let name = if count.get(base).copied().unwrap_or(0) > 1 {
            let n = seen.entry(base.to_owned()).or_default();
            *n += 1;
            format!("{base}{}", *n - 1)
        } else {
            base.to_owned()
        };
        for t in temps {
            out.push(Temp {
                name: name.clone(),
                driver: driver.clone(),
                label: t.label.clone(),
                celsius: round1(milli(t.input)),
                crit: t.crit.map(milli).filter(|c| CRIT_RANGE.contains(c)),
            });
        }
    }
    out.truncate(MAX_TEMPS);
    out
}

/// Temperatures and the first GPU with load or clock readings, from
/// `<sys>/class/hwmon`.
#[must_use]
pub fn read_hwmon(sys: &Path) -> (Vec<Temp>, Option<Gpu>) {
    let mut chips = Vec::new();
    let mut gpu = None;
    for dir in numbered_dirs(&sys.join("class/hwmon"), "hwmon") {
        let Some(driver) = read_trim(&dir.join("name")) else {
            continue;
        };
        let temps = numbered_files(&dir, "temp", "_input")
            .into_iter()
            .filter_map(|n| {
                Some(RawTemp {
                    input: read_num(&dir.join(format!("temp{n}_input")))?,
                    label: read_trim(&dir.join(format!("temp{n}_label"))),
                    crit: read_num(&dir.join(format!("temp{n}_crit"))),
                })
            })
            .collect();
        if gpu.is_none() {
            gpu = read_gpu(&dir, &driver);
        }
        chips.push((driver.clone(), chip_temps(&driver, temps)));
    }
    (name_temps(&chips), gpu)
}

fn read_gpu(dir: &Path, driver: &str) -> Option<Gpu> {
    let busy = read_num(&dir.join("device/gpu_busy_percent"))
        .and_then(|v| u8::try_from(v.clamp(0, 100)).ok());
    let mhz = numbered_files(dir, "freq", "_input")
        .into_iter()
        .find(|n| read_trim(&dir.join(format!("freq{n}_label"))).as_deref() == Some("sclk"))
        .and_then(|n| read_num(&dir.join(format!("freq{n}_input"))))
        .and_then(|hz| u32::try_from(hz / 1_000_000).ok());
    if busy.is_none() && mhz.is_none() {
        return None;
    }
    let watts = ["power1_average", "power1_input"]
        .iter()
        .find_map(|f| read_num(&dir.join(f)))
        .map(micro);
    Some(Gpu {
        driver: driver.to_owned(),
        busy,
        mhz,
        watts,
    })
}

// ── power source (power_supply) ────────────────────────────

/// One `power_supply` entry, as read. Values are the kernel's units (µW,
/// µA, µV, µWh).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawSupply {
    /// `type`: `Mains`, `USB`, `Battery`, `UPS`, …
    pub kind: String,
    pub online: Option<bool>,
    /// `Device` for peripherals' batteries (mouse, headset).
    pub scope: Option<String>,
    pub status: Option<String>,
    pub capacity: Option<i64>,
    pub power_now: Option<i64>,
    pub current_now: Option<i64>,
    pub voltage_now: Option<i64>,
    pub energy_now: Option<i64>,
    pub energy_full: Option<i64>,
}

/// Where the machine's power comes from. `None` when it has neither an
/// adapter nor a system battery (most desktops and servers).
#[must_use]
pub fn supply(entries: &[RawSupply]) -> Option<Supply> {
    let adapters: Vec<&RawSupply> = entries
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "Mains" | "USB"))
        .collect();
    let batteries: Vec<&RawSupply> = entries
        .iter()
        .filter(|s| s.kind == "Battery" && s.scope.as_deref() != Some("Device"))
        .collect();
    let battery = battery(&batteries);
    let on_ac = if adapters.iter().any(|a| a.online.is_some()) {
        adapters.iter().any(|a| a.online == Some(true))
    } else {
        // No adapter to ask: the battery says whether it is being drained.
        battery.as_ref()?.status != "discharging"
    };
    Some(Supply {
        source: if on_ac { Source::Ac } else { Source::Battery },
        battery,
    })
}

/// Several batteries (dual-battery laptops) add up: charge by energy when every
/// one reports it, else the mean capacity.
fn battery(all: &[&RawSupply]) -> Option<Battery> {
    if all.is_empty() {
        return None;
    }
    let status = |want: &str| {
        all.iter().any(|b| {
            b.status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(want))
        })
    };
    let status = if status("discharging") {
        "discharging".to_owned()
    } else if status("charging") {
        "charging".to_owned()
    } else {
        all[0]
            .status
            .as_deref()
            .unwrap_or("unknown")
            .to_ascii_lowercase()
    };
    let energy: Option<(i64, i64)> = all.iter().try_fold((0, 0), |(n, f), b| {
        Some((n + b.energy_now?, f + b.energy_full?.max(0)))
    });
    let capacity = match energy {
        Some((now, full)) if full > 0 => Some(now.saturating_mul(100) / full),
        _ => {
            let caps: Vec<i64> = all.iter().filter_map(|b| b.capacity).collect();
            let n = i64::try_from(caps.len()).ok().filter(|n| *n > 0)?;
            Some(caps.iter().sum::<i64>() / n)
        }
    }
    .and_then(|c| u8::try_from(c.clamp(0, 100)).ok());
    let rates: Vec<f64> = all
        .iter()
        .filter_map(|b| {
            // Some firmware reports a signed rate (negative = discharging).
            b.power_now.map(|p| micro(p.abs())).or_else(|| {
                let (i, v) = (b.current_now?, b.voltage_now?);
                #[allow(clippy::cast_precision_loss)]
                Some((i.abs() as f64 * v.abs() as f64) / 1e12)
            })
        })
        .collect();
    let watts = (!rates.is_empty()).then(|| round1(rates.iter().sum()));
    Some(Battery {
        capacity,
        status,
        watts,
    })
}

/// The power source, from `<sys>/class/power_supply`.
#[must_use]
pub fn read_supply(sys: &Path) -> Option<Supply> {
    let dir = sys.join("class/power_supply");
    let entries: Vec<RawSupply> = list(&dir)
        .into_iter()
        .filter_map(|(_, p)| {
            Some(RawSupply {
                kind: read_trim(&p.join("type"))?,
                online: read_num(&p.join("online")).map(|v| v != 0),
                scope: read_trim(&p.join("scope")),
                status: read_trim(&p.join("status")),
                capacity: read_num(&p.join("capacity")),
                power_now: read_num(&p.join("power_now")),
                current_now: read_num(&p.join("current_now")),
                voltage_now: read_num(&p.join("voltage_now")),
                energy_now: read_num(&p.join("energy_now")),
                energy_full: read_num(&p.join("energy_full")),
            })
        })
        .collect();
    supply(&entries)
}

// ── RAPL (powercap) ────────────────────────────────────────

/// Why there are (or are no) RAPL readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RaplStatus {
    /// The package energy counters are readable.
    Ok,
    /// The machine has no RAPL package domain (or the kernel module is
    /// not loaded).
    Absent,
    /// The counters exist but are root-only (the kernel's default since
    /// CVE-2020-8694).
    Denied,
    /// The host has them but they are not visible here: Docker masks
    /// `/sys/devices/virtual/powercap`, and the bind mount is missing.
    Masked,
}

/// One energy counter file and the value it wraps at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    pub path: PathBuf,
    /// `max_energy_range_uj`; 0 when unknown.
    pub max: u64,
}

/// One package domain (`intel-rapl:N` named `package-N`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub energy: Counter,
    /// Its `core` subdomain, when it has one.
    pub core: Option<Counter>,
    /// The `long_term` (PL1) constraint, in µW, when the kernel exposes it.
    pub limit_uw: Option<u64>,
}

/// Finds the package domains under `powercap` (either `/sys/class/powercap`
/// with its flat links or a mounted `/sys/devices/virtual/powercap` tree)
/// and says whether their counters can be read. `class` is
/// `<sys>/class/powercap`, consulted only to tell "masked" from "absent".
#[must_use]
pub fn probe_rapl(powercap: &Path, class: &Path) -> (RaplStatus, Vec<Package>) {
    let packages = rapl_packages(powercap);
    if packages.is_empty() {
        // Docker leaves the class links in place but hides their targets.
        let hidden = list(class)
            .iter()
            .any(|(name, _)| zone_index(name).is_some());
        let status = if hidden {
            RaplStatus::Masked
        } else {
            RaplStatus::Absent
        };
        return (status, packages);
    }
    let status = match packages
        .iter()
        .map(|p| read_counter(&p.energy.path))
        .find_map(Result::err)
    {
        None => RaplStatus::Ok,
        Some(e) if e.kind() == io::ErrorKind::PermissionDenied => RaplStatus::Denied,
        Some(_) => RaplStatus::Absent,
    };
    (status, packages)
}

/// `intel-rapl:N` → N. The MMIO interface (`intel-rapl-mmio:N`) reports
/// the same packages again and is left out, as are subdomains (`:N:M`).
fn zone_index(name: &str) -> Option<u32> {
    name.strip_prefix("intel-rapl:")?.parse().ok()
}

fn rapl_packages(root: &Path) -> Vec<Package> {
    // By zone name: the class dir lists a zone both directly and inside
    // its `intel-rapl` control-type link.
    let mut zones: BTreeMap<u32, PathBuf> = BTreeMap::new();
    for base in [root.to_owned(), root.join("intel-rapl")] {
        for (name, path) in list(&base) {
            if let Some(i) = zone_index(&name) {
                zones.entry(i).or_insert(path);
            }
        }
    }
    zones
        .into_values()
        .filter(|dir| read_trim(&dir.join("name")).is_some_and(|n| n.starts_with("package")))
        .map(|dir| Package {
            core: list(&dir)
                .into_iter()
                .filter(|(name, _)| name.starts_with("intel-rapl:"))
                .find(|(_, p)| read_trim(&p.join("name")).as_deref() == Some("core"))
                .map(|(_, p)| counter(&p)),
            limit_uw: long_term_limit(&dir),
            energy: counter(&dir),
        })
        .collect()
}

fn counter(dir: &Path) -> Counter {
    Counter {
        path: dir.join("energy_uj"),
        max: read_num(&dir.join("max_energy_range_uj"))
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
    }
}

fn long_term_limit(dir: &Path) -> Option<u64> {
    (0..8).find_map(|n| {
        let name = read_trim(&dir.join(format!("constraint_{n}_name")))?;
        (name == "long_term")
            .then(|| read_num(&dir.join(format!("constraint_{n}_power_limit_uw"))))
            .flatten()
            .and_then(|v| u64::try_from(v).ok())
            .filter(|v| *v > 0)
    })
}

/// Reads one `energy_uj`.
///
/// # Errors
/// The file's read error (permission denied is the usual one), or
/// `InvalidData` for contents that are not a number.
pub fn read_counter(path: &Path) -> io::Result<u64> {
    std::fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ── helpers ────────────────────────────────────────────────

#[allow(clippy::cast_precision_loss)]
fn milli(v: i64) -> f64 {
    v as f64 / 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn micro(v: i64) -> f64 {
    v as f64 / 1e6
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn read_trim(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

fn read_num(path: &Path) -> Option<i64> {
    read_trim(path)?.parse().ok()
}

/// A directory's entries by name, sorted, at most [`MAX_ENTRIES`].
fn list(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .take(MAX_ENTRIES)
        .collect();
    out.sort();
    out
}

/// `<prefix>N` entries of `dir` in numeric order (`hwmon2` before `hwmon10`).
fn numbered_dirs(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found: Vec<(u32, PathBuf)> = list(dir)
        .into_iter()
        .filter_map(|(name, path)| Some((name.strip_prefix(prefix)?.parse().ok()?, path)))
        .collect();
    found.sort_by_key(|(n, _)| *n);
    found.into_iter().map(|(_, p)| p).collect()
}

/// The N of every `<prefix>N<suffix>` file in `dir`, ascending.
fn numbered_files(dir: &Path, prefix: &str, suffix: &str) -> Vec<u32> {
    let mut found: Vec<u32> = list(dir)
        .into_iter()
        .filter_map(|(name, _)| {
            name.strip_prefix(prefix)?
                .strip_suffix(suffix)?
                .parse()
                .ok()
        })
        .collect();
    found.sort_unstable();
    found
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::system::tests::FakeProc;

    /// The dev laptop's sysfs (Ryzen 5 3500U, AC, no battery), under a
    /// temp dir: RAPL readable unless `denied` says otherwise.
    pub(crate) fn laptop(fake: &FakeProc) {
        let w = |rel: &str, v: &str| fake.write(rel, v);
        w("sys/class/hwmon/hwmon0/name", "ACAD\n");
        w("sys/class/hwmon/hwmon1/name", "nvme\n");
        w("sys/class/hwmon/hwmon1/temp1_input", "45850\n");
        w("sys/class/hwmon/hwmon1/temp1_label", "Composite\n");
        w("sys/class/hwmon/hwmon1/temp1_crit", "85850\n");
        w("sys/class/hwmon/hwmon2/name", "k10temp\n");
        w("sys/class/hwmon/hwmon2/temp1_input", "63875\n");
        w("sys/class/hwmon/hwmon2/temp1_label", "Tctl\n");
        w("sys/class/hwmon/hwmon3/name", "amdgpu\n");
        w("sys/class/hwmon/hwmon3/temp1_input", "63000\n");
        w("sys/class/hwmon/hwmon3/temp1_label", "edge\n");
        w("sys/class/hwmon/hwmon3/freq1_input", "200000000\n");
        w("sys/class/hwmon/hwmon3/freq1_label", "sclk\n");
        w("sys/class/hwmon/hwmon3/power1_label", "PPT\n");
        w("sys/class/hwmon/hwmon3/device/gpu_busy_percent", "17\n");
        w("sys/class/power_supply/ACAD/type", "Mains\n");
        w("sys/class/power_supply/ACAD/online", "1\n");
        rapl(fake, "sys/class/powercap", 1_000_000);
    }

    /// A class-style powercap dir with one package and its core.
    pub(crate) fn rapl(fake: &FakeProc, at: &str, energy: u64) {
        let z = format!("{at}/intel-rapl:0");
        fake.write(&format!("{z}/name"), "package-0\n");
        fake.write(&format!("{z}/max_energy_range_uj"), "65532610987\n");
        fake.write(&format!("{z}/energy_uj"), format!("{energy}\n"));
        fake.write(&format!("{z}/intel-rapl:0:0/name"), "core\n");
        fake.write(
            &format!("{z}/intel-rapl:0:0/max_energy_range_uj"),
            "65532610987\n",
        );
        fake.write(&format!("{z}/intel-rapl:0:0/energy_uj"), "100\n");
    }

    fn raw(label: &str, input: i64) -> RawTemp {
        RawTemp {
            label: Some(label.to_owned()),
            input,
            crit: None,
        }
    }

    #[test]
    fn friendly_names_by_driver() {
        assert_eq!(friendly("k10temp"), "cpu");
        assert_eq!(friendly("coretemp"), "cpu");
        assert_eq!(friendly("zenpower"), "cpu");
        assert_eq!(friendly("amdgpu"), "gpu");
        assert_eq!(friendly("nouveau"), "gpu");
        assert_eq!(friendly("nvme"), "nvme");
        assert_eq!(friendly("drivetemp"), "disk");
        assert_eq!(friendly("acpitz"), "acpi");
        assert_eq!(friendly("iwlwifi_1"), "iwlwifi_1");
    }

    #[test]
    fn cpu_chips_keep_the_package_reading_only() {
        let intel = vec![
            raw("Core 0", 50_000),
            raw("Package id 0", 55_000),
            raw("Core 1", 70_000),
        ];
        assert_eq!(chip_temps("coretemp", intel), [raw("Package id 0", 55_000)]);
        let amd = vec![
            raw("Tccd1", 60_000),
            raw("Tdie", 61_000),
            raw("Tctl", 71_000),
        ];
        assert_eq!(chip_temps("k10temp", amd), [raw("Tctl", 71_000)]);
        let unlabeled = vec![
            RawTemp {
                label: None,
                input: 40_000,
                crit: None,
            },
            raw("x", 1),
        ];
        assert_eq!(chip_temps("cpu_thermal", unlabeled)[0].input, 40_000);
        // Other chips keep every sensor; nonsense readings go.
        let gpu = vec![
            raw("edge", 60_000),
            raw("junction", 72_000),
            raw("mem", -273_000),
        ];
        assert_eq!(chip_temps("amdgpu", gpu).len(), 2);
    }

    #[test]
    fn shared_friendly_names_are_numbered() {
        let crit = |label: &str, input, crit| RawTemp {
            label: Some(label.to_owned()),
            input,
            crit: Some(crit),
        };
        let temps = name_temps(&[
            ("nvme".to_owned(), vec![crit("Composite", 45_850, 85_850)]),
            ("k10temp".to_owned(), vec![raw("Tctl", 63_875)]),
            ("ACAD".to_owned(), vec![]),
            ("nvme".to_owned(), vec![crit("Composite", 39_000, 255_000)]),
        ]);
        let got: Vec<(&str, &str, f64, Option<f64>)> = temps
            .iter()
            .map(|t| (t.name.as_str(), t.driver.as_str(), t.celsius, t.crit))
            .collect();
        assert_eq!(
            got,
            [
                ("nvme0", "nvme", 45.9, Some(85.85)),
                ("cpu", "k10temp", 63.9, None),
                ("nvme1", "nvme", 39.0, None),
            ],
            "placeholder crit (255 °C) dropped"
        );
    }

    #[test]
    fn hwmon_of_the_dev_laptop() {
        let fake = FakeProc::new();
        laptop(&fake);
        let (temps, gpu) = read_hwmon(&fake.0.join("sys"));
        let got: Vec<(&str, Option<&str>, f64)> = temps
            .iter()
            .map(|t| (t.name.as_str(), t.label.as_deref(), t.celsius))
            .collect();
        assert_eq!(
            got,
            [
                ("nvme", Some("Composite"), 45.9),
                ("cpu", Some("Tctl"), 63.9),
                ("gpu", Some("edge"), 63.0),
            ]
        );
        assert_eq!(
            gpu,
            Some(Gpu {
                driver: "amdgpu".to_owned(),
                busy: Some(17),
                mhz: Some(200),
                watts: None,
            })
        );
        assert_eq!(
            read_supply(&fake.0.join("sys")),
            Some(Supply {
                source: Source::Ac,
                battery: None
            })
        );
    }

    #[test]
    fn nothing_to_read_is_nothing_to_show() {
        let fake = FakeProc::new();
        let sys = fake.0.join("sys");
        assert_eq!(read_hwmon(&sys), (vec![], None));
        assert_eq!(read_supply(&sys), None);
    }

    fn bat(status: &str) -> RawSupply {
        RawSupply {
            kind: "Battery".to_owned(),
            status: Some(status.to_owned()),
            capacity: Some(83),
            ..RawSupply::default()
        }
    }

    fn mains(online: bool) -> RawSupply {
        RawSupply {
            kind: "Mains".to_owned(),
            online: Some(online),
            ..RawSupply::default()
        }
    }

    #[test]
    fn battery_discharging_with_power_now() {
        let mut b = bat("Discharging");
        b.power_now = Some(7_200_000);
        let s = supply(&[mains(false), b]).unwrap();
        assert_eq!(s.source, Source::Battery);
        assert_eq!(
            s.battery,
            Some(Battery {
                capacity: Some(83),
                status: "discharging".to_owned(),
                watts: Some(7.2),
            })
        );
    }

    #[test]
    fn battery_watts_from_current_and_voltage() {
        let mut b = bat("Charging");
        // 1.5 A at 12 V, reported as a negative current by some firmware.
        b.current_now = Some(-1_500_000);
        b.voltage_now = Some(12_000_000);
        let s = supply(&[mains(true), b]).unwrap();
        assert_eq!(s.source, Source::Ac);
        let b = s.battery.unwrap();
        assert_eq!((b.status.as_str(), b.watts), ("charging", Some(18.0)));
    }

    #[test]
    fn two_batteries_add_up_by_energy() {
        let mut a = bat("Discharging");
        (a.energy_now, a.energy_full) = (Some(10_000_000), Some(20_000_000));
        a.power_now = Some(3_000_000);
        let mut b = bat("Unknown");
        (b.energy_now, b.energy_full) = (Some(20_000_000), Some(20_000_000));
        b.power_now = Some(-1_000_000);
        let s = supply(&[a, b]).unwrap();
        assert_eq!(s.source, Source::Battery, "no adapter: the battery says");
        let b = s.battery.unwrap();
        assert_eq!(
            (b.capacity, b.status.as_str(), b.watts),
            (Some(75), "discharging", Some(4.0))
        );
    }

    #[test]
    fn peripheral_batteries_and_desktops() {
        let mut mouse = bat("Discharging");
        mouse.scope = Some("Device".to_owned());
        assert_eq!(
            supply(&[mouse.clone()]),
            None,
            "a mouse is not the machine's"
        );
        let s = supply(&[mains(true), mouse]).unwrap();
        assert_eq!((s.source, s.battery), (Source::Ac, None));
        assert_eq!(supply(&[]), None);
        let usb = RawSupply {
            kind: "USB".to_owned(),
            online: Some(true),
            ..RawSupply::default()
        };
        let s = supply(&[mains(false), usb, bat("Charging")]).unwrap();
        assert_eq!(s.source, Source::Ac, "USB-C charger");
        let s = supply(&[bat("Full")]).unwrap();
        assert_eq!(s.source, Source::Ac);
    }

    #[test]
    fn rapl_readable_in_class_layout() {
        let fake = FakeProc::new();
        rapl(&fake, "sys/class/powercap", 5);
        fake.write(
            "sys/class/powercap/intel-rapl:0/constraint_0_name",
            "long_term\n",
        );
        fake.write(
            "sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
            "15000000\n",
        );
        fake.write(
            "sys/class/powercap/intel-rapl:0/constraint_1_name",
            "short_term\n",
        );
        // The same package again over MMIO: not counted twice.
        fake.write("sys/class/powercap/intel-rapl-mmio:0/name", "package-0\n");
        let class = fake.0.join("sys/class/powercap");
        let (status, pkgs) = probe_rapl(&class, &class);
        assert_eq!(status, RaplStatus::Ok);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].energy.max, 65_532_610_987);
        assert_eq!(pkgs[0].limit_uw, Some(15_000_000));
        let core = pkgs[0].core.as_ref().unwrap();
        assert_eq!(read_counter(&core.path).unwrap(), 100);
    }

    #[test]
    fn rapl_in_a_mounted_devices_tree() {
        let fake = FakeProc::new();
        rapl(&fake, "host/powercap/intel-rapl", 5);
        // A psys domain is not a package.
        fake.write("host/powercap/intel-rapl/intel-rapl:1/name", "psys\n");
        fake.write("host/powercap/intel-rapl/intel-rapl:1/energy_uj", "9\n");
        let (status, pkgs) = probe_rapl(
            &fake.0.join("host/powercap"),
            &fake.0.join("sys/class/powercap"),
        );
        assert_eq!((status, pkgs.len()), (RaplStatus::Ok, 1));
        assert_eq!(pkgs[0].limit_uw, None);
    }

    #[test]
    fn rapl_absent_or_masked() {
        let fake = FakeProc::new();
        let class = fake.0.join("sys/class/powercap");
        assert_eq!(probe_rapl(&class, &class).0, RaplStatus::Absent);
        // What a container sees: the link is there, its target is not.
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(fake.0.join("gone"), class.join("intel-rapl:0")).unwrap();
        assert_eq!(probe_rapl(&class, &class).0, RaplStatus::Masked);
        assert_eq!(
            probe_rapl(&fake.0.join("host/powercap"), &class).0,
            RaplStatus::Masked,
            "CUTHULU_POWERCAP_DIR set but nothing mounted there"
        );
    }

    #[test]
    fn rapl_root_only_is_denied() {
        use std::os::unix::fs::PermissionsExt;
        let fake = FakeProc::new();
        rapl(&fake, "sys/class/powercap", 5);
        let file = fake.0.join("sys/class/powercap/intel-rapl:0/energy_uj");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let class = fake.0.join("sys/class/powercap");
        let status = probe_rapl(&class, &class).0;
        // Root reads anything; the test can only check the mapping as a user.
        if std::fs::read(&file).is_err() {
            assert_eq!(status, RaplStatus::Denied);
        }
    }
}
