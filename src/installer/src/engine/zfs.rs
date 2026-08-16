//! Pool and dataset creation.
//!
//! Two pools, because GRUB's ZFS reader only understands a subset of pool
//! features. bpool is created with `compatibility=grub2` so GRUB can read
//! /boot; rpool is a normal modern pool with zstd. Putting /boot in rpool
//! instead would mean crippling the root pool's feature set forever.

use super::Stepper;
use crate::cmd::Cmd;
use crate::config::*;
use crate::error::{IoContext, Result};
use crate::logging;

/// Export pools left imported by an earlier failed attempt.
///
/// Without this, a second run fails at `zpool create` with "pool already
/// exists" and the operator is left to work out that they need to export by
/// hand. Best-effort: if there is nothing to export, there is nothing to do.
pub fn release_existing_pools(step: &mut Stepper) {
    for pool in [BPOOL, RPOOL] {
        let listed = Cmd::new("zpool")
            .arg("list")
            .arg("-H")
            .arg("-o")
            .arg("name")
            .arg(pool)
            .run_lenient();

        if matches!(listed, Ok(ref o) if o.ok()) {
            step.warn(format!(
                "{pool} is imported from an earlier attempt; exporting it"
            ));
            let _ = Cmd::new("zfs").arg("unmount").arg("-a").run_lenient();
            let _ = Cmd::new("zpool").arg("export").arg("-f").arg(pool).run_lenient();
        }
    }
}

pub fn create_bpool(config: &InstallConfig) -> Result<()> {
    Cmd::new("zpool")
        .arg("create")
        .arg("-f")
        // Mandatory. GRUB refuses to read a pool using features it does not
        // implement, and it implements very few.
        .arg("-o")
        .arg("compatibility=grub2")
        .arg("-o")
        .arg("ashift=12")
        .arg("-o")
        .arg("autotrim=on")
        // No device nodes on the boot pool; nothing there needs them.
        .arg("-O")
        .arg("devices=off")
        .arg("-O")
        .arg("acltype=posixacl")
        .arg("-O")
        .arg("xattr=sa")
        // lz4, not zstd: zstd_compress is not in the grub2 feature set.
        .arg("-O")
        .arg("compression=lz4")
        .arg("-O")
        .arg("relatime=on")
        .arg("-O")
        .arg("canmount=off")
        .arg("-O")
        .arg("mountpoint=/boot")
        // Assemble under an alternate root: every mountpoint property is the
        // final one, but nothing lands on the live system's own directories.
        .arg("-R")
        .arg(TARGET)
        .arg(BPOOL)
        .args(config.vdev_args(PART_BPOOL))
        .run()?;
    Ok(())
}

pub fn create_rpool(config: &InstallConfig) -> Result<()> {
    Cmd::new("zpool")
        .arg("create")
        .arg("-f")
        .arg("-o")
        .arg("ashift=12")
        .arg("-o")
        .arg("autotrim=on")
        .arg("-O")
        .arg("acltype=posixacl")
        .arg("-O")
        .arg("xattr=sa")
        .arg("-O")
        .arg("dnodesize=auto")
        .arg("-O")
        .arg("compression=zstd")
        .arg("-O")
        .arg("atime=off")
        .arg("-O")
        .arg("canmount=off")
        .arg("-O")
        .arg("mountpoint=/")
        .arg("-R")
        .arg(TARGET)
        .arg(RPOOL)
        .args(config.vdev_args(PART_RPOOL))
        .run()?;
    Ok(())
}

pub fn create_datasets(step: &mut Stepper) -> Result<()> {
    // Root filesystem. canmount=noauto keeps anything other than the initramfs
    // from mounting it: with two boot environments under rpool/ROOT you do not
    // want both mounting themselves at / on import.
    create(&["-o", "canmount=off", "-o", "mountpoint=none", "rpool/ROOT"])?;
    create(&["-o", "canmount=noauto", "-o", "mountpoint=/", ROOT_DATASET])?;
    Cmd::new("zfs").arg("mount").arg(ROOT_DATASET).run()?;
    step.detail(format!("{ROOT_DATASET} mounted at {TARGET}"));

    // Boot filesystem, on top of the now-mounted root.
    create(&["-o", "canmount=off", "-o", "mountpoint=none", "bpool/BOOT"])?;
    create(&["-o", "mountpoint=/boot", BOOT_DATASET])?;
    step.detail(format!("{BOOT_DATASET} mounted at {TARGET}/boot"));

    // /var/log and /var/tmp as their own datasets, so log growth cannot fill
    // the root dataset and /var/tmp can have its own policy later.
    // The /var container is canmount=off: /var itself stays part of the root
    // dataset, only the two children are separate.
    create(&["-o", "canmount=off", &format!("{ROOT_DATASET}/var")])?;
    create(&[&format!("{ROOT_DATASET}/var/log")])?;
    create(&[&format!("{ROOT_DATASET}/var/tmp")])?;
    step.detail("separate datasets for /var/log and /var/tmp");

    // A fresh dataset is 0755 root:root; /var/tmp has to be sticky and
    // world-writable or half of userspace breaks quietly.
    Cmd::new("chmod")
        .arg("1777")
        .arg(format!("{TARGET}/var/tmp"))
        .run()?;

    Ok(())
}

fn create(args: &[&str]) -> Result<()> {
    Cmd::new("zfs").arg("create").args(args.to_vec()).run()?;
    Ok(())
}

/// Write the pool cache and copy it into the target.
///
/// Without a cachefile the new system has to scan every device to find its
/// pools at boot. With one, zfs-import-cache.service imports exactly these two
/// pools, which is both faster and far more predictable on a machine that may
/// have other disks in it.
pub fn write_cachefile(step: &mut Stepper) -> Result<()> {
    for pool in [BPOOL, RPOOL] {
        Cmd::new("zpool")
            .arg("set")
            .arg("cachefile=/etc/zfs/zpool.cache")
            .arg(pool)
            .run()?;
    }

    let src = std::path::Path::new("/etc/zfs/zpool.cache");
    let dst_dir = format!("{TARGET}/etc/zfs");
    let dst = format!("{dst_dir}/zpool.cache");

    std::fs::create_dir_all(&dst_dir).ctx(format!("creating {dst_dir}"))?;
    std::fs::copy(src, &dst).ctx(format!("copying the pool cache to {dst}"))?;

    step.detail("pool cache written to /etc/zfs/zpool.cache");
    logging::info(format!("cachefile copied to {dst}"));
    Ok(())
}
