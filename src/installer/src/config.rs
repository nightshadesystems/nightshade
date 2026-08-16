//! What the operator chose, and the on-disk layout it implies.

use crate::disk::Disk;
use crate::secret::SecretString;

// --- pools and datasets ---------------------------------------------------

pub const BPOOL: &str = "bpool";
pub const RPOOL: &str = "rpool";
pub const BOOT_DATASET: &str = "bpool/BOOT/nightshade";
pub const ROOT_DATASET: &str = "rpool/ROOT/nightshade";

/// Alternate root the pools are imported under during installation. Using -R
/// keeps every dataset's mountpoint property relative to the final system
/// while it is assembled somewhere else entirely.
pub const TARGET: &str = "/mnt/nightshade";

// --- partition layout -----------------------------------------------------

/// p1: EFI System Partition. 512 MiB is far more than GRUB needs, but it is
/// the size every firmware is known to be happy with and leaves room for a
/// second bootloader later.
pub const ESP_SIZE: &str = "+512M";
/// p2: boot pool. 2 GiB holds a good number of kernel/initramfs pairs; the
/// initramfs alone is ~40 MiB.
pub const BPOOL_SIZE: &str = "+2G";

pub const PART_ESP: u32 = 1;
pub const PART_BPOOL: u32 = 2;
pub const PART_RPOOL: u32 = 3;

/// GPT type codes (sgdisk shorthand).
pub const TYPE_ESP: &str = "EF00";
pub const TYPE_BPOOL: &str = "BE00";
pub const TYPE_RPOOL: &str = "BF00";

/// Warn above this relative size difference between mirror members.
pub const MIRROR_SIZE_TOLERANCE: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One disk, single-vdev pools.
    Single,
    /// Two disks, mirrored vdevs in both pools.
    Mirror,
}

impl Layout {
    pub fn label(&self) -> &'static str {
        match self {
            Layout::Single => "single disk",
            Layout::Mirror => "ZFS RAID1 mirror (2 disks)",
        }
    }
}

pub struct InstallConfig {
    /// One disk, or exactly two for a mirror.
    pub disks: Vec<Disk>,
    pub hostname: String,
    pub password: SecretString,
}

impl InstallConfig {
    pub fn layout(&self) -> Layout {
        if self.disks.len() >= 2 {
            Layout::Mirror
        } else {
            Layout::Single
        }
    }

    pub fn is_mirror(&self) -> bool {
        self.layout() == Layout::Mirror
    }

    /// vdev specification for `zpool create` -- either a bare device or a
    /// `mirror a b` triple.
    pub fn vdev_args(&self, partition: u32) -> Vec<String> {
        let members: Vec<String> = self
            .disks
            .iter()
            .map(|d| d.partition(partition).to_string_lossy().into_owned())
            .collect();

        if self.is_mirror() {
            let mut v = vec!["mirror".to_string()];
            v.extend(members);
            v
        } else {
            members
        }
    }

    /// Mount point for disk `index`'s ESP inside the target.
    ///
    /// The first ESP is the real /boot/efi. The second is mounted alongside it
    /// only so GRUB can be installed onto it, and is kept in step afterwards by
    /// the ESP sync unit rather than by being mounted at boot -- two
    /// filesystems mounted at once with the same UUID is a good way to have the
    /// wrong one win.
    pub fn esp_mountpoint(&self, index: usize) -> String {
        if index == 0 {
            format!("{TARGET}/boot/efi")
        } else {
            format!("{TARGET}/boot/efi{}", index + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn disk(name: &str) -> Disk {
        Disk {
            name: name.into(),
            path: PathBuf::from(format!("/dev/{name}")),
            stable_path: PathBuf::from(format!("/dev/disk/by-id/virtio-{name}")),
            size_bytes: 20 * 1024 * 1024 * 1024,
            model: "test".into(),
            serial: "test".into(),
            rotational: false,
            removable: false,
            signatures: Vec::new(),
        }
    }

    fn config(names: &[&str]) -> InstallConfig {
        InstallConfig {
            disks: names.iter().map(|n| disk(n)).collect(),
            hostname: "nightshade".into(),
            password: SecretString::new("password".into()),
        }
    }

    #[test]
    fn single_disk_vdev_is_a_bare_device() {
        let c = config(&["vda"]);
        assert_eq!(c.layout(), Layout::Single);
        assert_eq!(c.vdev_args(PART_RPOOL), vec!["/dev/disk/by-id/virtio-vda-part3"]);
    }

    #[test]
    fn two_disks_produce_a_mirror_vdev() {
        let c = config(&["vda", "vdb"]);
        assert_eq!(c.layout(), Layout::Mirror);
        assert_eq!(
            c.vdev_args(PART_BPOOL),
            vec![
                "mirror",
                "/dev/disk/by-id/virtio-vda-part2",
                "/dev/disk/by-id/virtio-vdb-part2"
            ]
        );
    }

    #[test]
    fn esp_mountpoints_are_distinct() {
        let c = config(&["vda", "vdb"]);
        assert_eq!(c.esp_mountpoint(0), format!("{TARGET}/boot/efi"));
        assert_eq!(c.esp_mountpoint(1), format!("{TARGET}/boot/efi2"));
    }
}
