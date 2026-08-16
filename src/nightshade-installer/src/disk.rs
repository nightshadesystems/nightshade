//! Block device discovery.
//!
//! Everything here reads /sys directly rather than shelling out, because the
//! disk list is what the operator stakes their data on and /sys is the kernel's
//! own answer. The one exception is the per-disk signature summary, which uses
//! lsblk purely to render existing partitions for the confirmation screen.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cmd::Cmd;
use crate::error::{Error, IoContext, Result};
use crate::logging;

#[derive(Debug, Clone)]
pub struct Disk {
    /// Kernel name, e.g. "sda", "nvme0n1", "vda".
    pub name: String,
    /// /dev/<name>.
    pub path: PathBuf,
    /// /dev/disk/by-id/... when one exists, otherwise the same as `path`.
    ///
    /// Pools are always created against this. A mirror built on /dev/sda and
    /// /dev/sdb survives right up until the kernel enumerates them in the other
    /// order after a reboot; by-id names do not move.
    pub stable_path: PathBuf,
    pub size_bytes: u64,
    pub model: String,
    pub serial: String,
    pub rotational: bool,
    pub removable: bool,
    /// Rendered existing partition table, one line per partition. Empty when
    /// the disk has no recognisable partitions.
    pub signatures: Vec<String>,
}

impl Disk {
    pub fn size_human(&self) -> String {
        human_bytes(self.size_bytes)
    }

    /// Path to partition `n`, respecting the naming rule for whichever style of
    /// device path we are using.
    pub fn partition(&self, n: u32) -> PathBuf {
        let s = self.stable_path.to_string_lossy().into_owned();
        if s.starts_with("/dev/disk/by-id/") {
            // udev's by-id partition links are <id>-partN.
            PathBuf::from(format!("{s}-part{n}"))
        } else if s.ends_with(|c: char| c.is_ascii_digit()) {
            // nvme0n1 -> nvme0n1p1, mmcblk0 -> mmcblk0p1
            PathBuf::from(format!("{s}p{n}"))
        } else {
            PathBuf::from(format!("{s}{n}"))
        }
    }

    /// Kernel-name partition path, for the few places that need the real node
    /// rather than a symlink (mount, mkfs on a freshly created table).
    pub fn kernel_partition(&self, n: u32) -> PathBuf {
        let s = self.path.to_string_lossy().into_owned();
        if s.ends_with(|c: char| c.is_ascii_digit()) {
            PathBuf::from(format!("{s}p{n}"))
        } else {
            PathBuf::from(format!("{s}{n}"))
        }
    }

    /// Short media descriptor for the disk picker.
    ///
    /// "removable" is worth surfacing: a USB stick is a legal install target
    /// and occasionally the intended one, but far more often it is the disk
    /// the operator did not mean to pick.
    pub fn media(&self) -> String {
        let kind = if self.rotational { "HDD" } else { "SSD" };
        if self.removable {
            format!("{kind}, removable")
        } else {
            kind.to_string()
        }
    }
}

/// Device name prefixes that are never installation targets.
///
/// loop/ram/zram are virtual; sr is optical; dm/md are stacked devices we would
/// be destroying from the wrong layer; zd is a ZFS zvol, which on a live system
/// means we would be installing into a pool we are about to import.
const SKIP_PREFIXES: &[&str] = &[
    "loop", "ram", "zram", "sr", "fd", "dm-", "md", "nbd", "zd",
];

pub fn enumerate() -> Result<Vec<Disk>> {
    let live = live_medium_disks();
    if !live.is_empty() {
        logging::info(format!("live medium backed by: {live:?}"));
    } else {
        logging::warn("could not identify the live medium; no disk will be hidden");
    }

    let mut disks = Vec::new();
    let entries = fs::read_dir("/sys/block").ctx("cannot read /sys/block")?;

    for entry in entries {
        let entry = entry.ctx("cannot read /sys/block entry")?;
        let name = entry.file_name().to_string_lossy().into_owned();

        if SKIP_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        if live.contains(&name) {
            logging::info(format!("hiding {name}: it is the live medium"));
            continue;
        }

        let sys = entry.path();
        let sectors = read_u64(&sys.join("size")).unwrap_or(0);
        if sectors == 0 {
            // Empty card readers and similar present as zero-size devices.
            continue;
        }

        disks.push(Disk {
            size_bytes: sectors * 512,
            model: read_str(&sys.join("device/model"))
                .or_else(|| read_str(&sys.join("device/name")))
                .unwrap_or_else(|| "unknown model".into()),
            serial: read_serial(&sys, &name),
            rotational: read_str(&sys.join("queue/rotational")).as_deref() == Some("1"),
            removable: read_str(&sys.join("removable")).as_deref() == Some("1"),
            stable_path: stable_path(&name),
            signatures: signatures(&PathBuf::from(format!("/dev/{name}"))),
            path: PathBuf::from(format!("/dev/{name}")),
            name,
        });
    }

    disks.sort_by(|a, b| a.name.cmp(&b.name));
    logging::info(format!("{} installable disk(s) found", disks.len()));
    Ok(disks)
}

/// Kernel names of the disks backing the live medium.
///
/// live-boot mounts the boot medium at /run/live/medium and the squashfs from a
/// loop device on top of it. Either can identify the disk, so both are chased:
/// a loop mount is resolved through its backing file to whatever filesystem
/// holds that file, and a partition is resolved up to its parent disk.
fn live_medium_disks() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return found;
    };

    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(source), Some(target)) = (f.next(), f.next()) else {
            continue;
        };

        let interesting = target.starts_with("/run/live/")
            || target == "/run/live"
            || target == "/lib/live/mount/medium"
            || target.starts_with("/cdrom");
        if !interesting {
            continue;
        }

        if let Some(disk) = resolve_to_disk(source) {
            found.insert(disk);
        }
    }

    found
}

/// Walk a device path back to the whole-disk kernel name it lives on.
fn resolve_to_disk(source: &str) -> Option<String> {
    let name = Path::new(source).file_name()?.to_string_lossy().into_owned();

    // A loop device is backed by a file; find the filesystem holding that file.
    if name.starts_with("loop") {
        let backing = fs::read_to_string(format!("/sys/block/{name}/loop/backing_file")).ok()?;
        let backing = backing.trim();
        let holder = mount_source_for_path(backing)?;
        // Guard against a loop-on-loop cycle rather than recursing forever.
        if holder.contains("loop") {
            return None;
        }
        return resolve_to_disk(&holder);
    }

    // A partition points at its parent disk through its sysfs parent directory.
    let sys = PathBuf::from(format!("/sys/class/block/{name}"));
    if sys.join("partition").exists() {
        let real = fs::canonicalize(&sys).ok()?;
        let parent = real.parent()?.file_name()?.to_string_lossy().into_owned();
        return Some(parent);
    }

    if sys.exists() { Some(name) } else { None }
}

/// The device whose mount point is the longest prefix of `path`.
fn mount_source_for_path(path: &str) -> Option<String> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mut best: Option<(usize, String)> = None;

    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(source), Some(target)) = (f.next(), f.next()) else {
            continue;
        };
        if !path.starts_with(target) {
            continue;
        }
        // "/run" must not match "/runner"; require a boundary.
        if target != "/" && path.len() > target.len() && !path[target.len()..].starts_with('/') {
            continue;
        }
        if best.as_ref().is_none_or(|(len, _)| target.len() > *len) {
            best = Some((target.len(), source.to_string()));
        }
    }

    best.map(|(_, s)| s)
}

/// Prefer a /dev/disk/by-id name; see the note on `Disk::stable_path`.
fn stable_path(name: &str) -> PathBuf {
    let dev = PathBuf::from(format!("/dev/{name}"));
    let Ok(entries) = fs::read_dir("/dev/disk/by-id") else {
        return dev;
    };

    let mut candidates: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let link = entry.path();
        let Ok(target) = fs::canonicalize(&link) else {
            continue;
        };
        if target != dev {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        // "-part1" links point at partitions, not the disk.
        if id.contains("-part") {
            continue;
        }
        candidates.push(id);
    }

    // wwn- ids are stable but opaque; a bus id names the hardware in a way the
    // operator can match against a physical label, so prefer it.
    candidates.sort_by_key(|id| (id.starts_with("wwn-"), id.len()));
    match candidates.first() {
        Some(id) => PathBuf::from(format!("/dev/disk/by-id/{id}")),
        None => dev,
    }
}

fn read_serial(sys: &Path, name: &str) -> String {
    for rel in ["serial", "device/serial", "device/vpd_pg80"] {
        if let Some(v) = read_str(&sys.join(rel)) {
            let v: String = v
                .chars()
                .filter(|c| c.is_ascii_graphic() || *c == ' ')
                .collect();
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    // Fall back to whatever udev encoded into the by-id name.
    let stable = stable_path(name);
    if let Some(id) = stable.file_name() {
        let id = id.to_string_lossy();
        if let Some((_, tail)) = id.split_once('-') {
            return tail.to_string();
        }
    }
    "no serial".into()
}

/// Existing partitions, for the "this is what will be destroyed" screen.
fn signatures(dev: &Path) -> Vec<String> {
    let out = Cmd::new("lsblk")
        .arg("--noheadings")
        .arg("--output")
        .arg("NAME,SIZE,FSTYPE,LABEL,PARTLABEL")
        .arg(dev.to_string_lossy().into_owned())
        .run_lenient();

    match out {
        // Line 1 is the disk itself; the rest are its partitions.
        Ok(o) if o.ok() => o.lines().skip(1).map(|l| l.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn read_str(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn read_u64(path: &Path) -> Option<u64> {
    read_str(path)?.parse().ok()
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit <= 1 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Two disks in a mirror should be the same size. They do not have to be --
/// ZFS just sizes the vdev to the smaller member -- but a large mismatch
/// usually means the operator picked the wrong device.
pub fn size_mismatch_ratio(a: &Disk, b: &Disk) -> f64 {
    let (small, large) = if a.size_bytes <= b.size_bytes {
        (a.size_bytes, b.size_bytes)
    } else {
        (b.size_bytes, a.size_bytes)
    };
    if large == 0 {
        return 0.0;
    }
    1.0 - (small as f64 / large as f64)
}

/// Refuse to continue if a tool we depend on is missing, before anything is
/// written to a disk.
pub fn require_tools(tools: &[&str]) -> Result<()> {
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|t| !crate::cmd::exists(t))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::env(format!(
            "missing required tools: {}",
            missing.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, stable: &str, size: u64) -> Disk {
        Disk {
            name: name.into(),
            path: PathBuf::from(format!("/dev/{name}")),
            stable_path: PathBuf::from(stable),
            size_bytes: size,
            model: "test".into(),
            serial: "test".into(),
            rotational: false,
            removable: false,
            signatures: Vec::new(),
        }
    }

    #[test]
    fn partition_naming_by_id() {
        let d = disk("vda", "/dev/disk/by-id/virtio-NSDISK0", 0);
        assert_eq!(
            d.partition(2),
            PathBuf::from("/dev/disk/by-id/virtio-NSDISK0-part2")
        );
    }

    #[test]
    fn partition_naming_letter_device() {
        let d = disk("sda", "/dev/sda", 0);
        assert_eq!(d.partition(1), PathBuf::from("/dev/sda1"));
        assert_eq!(d.kernel_partition(1), PathBuf::from("/dev/sda1"));
    }

    #[test]
    fn partition_naming_digit_terminated_device() {
        // The rule that catches nvme and mmc: a trailing digit needs a "p".
        let d = disk("nvme0n1", "/dev/nvme0n1", 0);
        assert_eq!(d.partition(3), PathBuf::from("/dev/nvme0n1p3"));
        assert_eq!(d.kernel_partition(3), PathBuf::from("/dev/nvme0n1p3"));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(20 * 1024 * 1024 * 1024), "20.0 GiB");
        assert_eq!(human_bytes(2 * 1024_u64.pow(4)), "2.0 TiB");
    }

    #[test]
    fn mismatch_ratio() {
        let a = disk("a", "/dev/a", 1000);
        let b = disk("b", "/dev/b", 1000);
        assert!(size_mismatch_ratio(&a, &b) < f64::EPSILON);

        let c = disk("c", "/dev/c", 900);
        // 10% smaller sits exactly on the warning threshold.
        assert!((size_mismatch_ratio(&a, &c) - 0.10).abs() < 1e-9);
    }
}
