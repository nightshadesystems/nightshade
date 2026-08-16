//! GPT partitioning, driven through sgdisk.
//!
//! Layout per disk, identical whether it is a single-disk install or one half
//! of a mirror:
//!
//!   p1  512 MiB  EF00  EFI System Partition, FAT32
//!   p2    2 GiB  BE00  bpool member  (GRUB-readable boot pool)
//!   p3  rest     BF00  rpool member  (root pool)

use std::path::Path;
use std::time::{Duration, Instant};

use crate::cmd::Cmd;
use crate::config::*;
use crate::disk::Disk;
use crate::error::{Error, Result};
use crate::logging;

/// Wipe `disk` and lay down the Nightshade partition table.
pub fn prepare(disk: &Disk) -> Result<()> {
    let dev = disk.path.to_string_lossy().into_owned();

    // --zap-all clears both the GPT and any MBR hiding underneath it. wipefs
    // then removes filesystem and pool signatures that live inside what were
    // partitions, which sgdisk does not touch -- leaving an old ZFS label
    // behind makes `zpool import` later find a pool that should not exist.
    Cmd::new("sgdisk").arg("--zap-all").arg(&dev).run()?;
    Cmd::new("wipefs").arg("--all").arg(&dev).run()?;

    // Start sectors of 0 mean "first aligned free sector"; sgdisk's default
    // 1 MiB alignment is what we want on every device we care about.
    Cmd::new("sgdisk")
        .arg(format!("-n{PART_ESP}:0:{ESP_SIZE}"))
        .arg(format!("-t{PART_ESP}:{TYPE_ESP}"))
        .arg(format!("-c{PART_ESP}:EFI System"))
        .arg(format!("-n{PART_BPOOL}:0:{BPOOL_SIZE}"))
        .arg(format!("-t{PART_BPOOL}:{TYPE_BPOOL}"))
        .arg(format!("-c{PART_BPOOL}:nightshade boot"))
        .arg(format!("-n{PART_RPOOL}:0:0"))
        .arg(format!("-t{PART_RPOOL}:{TYPE_RPOOL}"))
        .arg(format!("-c{PART_RPOOL}:nightshade root"))
        .arg(&dev)
        .run()?;

    Ok(())
}

/// Wait until the kernel and udev have published every partition we created.
///
/// sgdisk asks the kernel to re-read the table, but the device nodes and
/// especially the /dev/disk/by-id symlinks appear asynchronously. Creating a
/// pool against a path that does not exist yet is a genuinely intermittent
/// failure, so wait for the real thing rather than sleeping and hoping.
pub fn settle(config: &crate::config::InstallConfig) -> Result<()> {
    for disk in &config.disks {
        let dev = disk.path.to_string_lossy().into_owned();
        // Belt and braces: ask the kernel again in case sgdisk's request was
        // refused because something briefly held the device open.
        let _ = Cmd::new("blockdev").arg("--rereadpt").arg(&dev).run_lenient();
    }

    let _ = Cmd::new("udevadm").arg("settle").arg("--timeout=30").run_lenient();

    let deadline = Instant::now() + Duration::from_secs(30);
    for disk in &config.disks {
        for part in [PART_ESP, PART_BPOOL, PART_RPOOL] {
            wait_for(&disk.partition(part), deadline)?;
            wait_for(&disk.kernel_partition(part), deadline)?;
        }
    }

    logging::info("all partitions present");
    Ok(())
}

fn wait_for(path: &Path, deadline: Instant) -> Result<()> {
    loop {
        // exists() follows symlinks, which is exactly the check we want for a
        // by-id link: the link is useless until its target is there too.
        if path.exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::env(format!(
                "timed out waiting for {} to appear after partitioning",
                path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
