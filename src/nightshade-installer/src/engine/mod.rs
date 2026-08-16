//! The install engine.
//!
//! Owns the flow and every destructive action. Frontends are asked questions
//! and told about progress; they decide nothing. The order of screens here is
//! the specified order, and there is no branch that skips the password or the
//! typed ERASE confirmation.

pub mod boot;
pub mod partition;
pub mod rootfs;
pub mod target;
pub mod zfs;

use std::path::Path;

use crate::cmd::Cmd;
use crate::config::{self, InstallConfig, Layout};
use crate::disk::{self, Disk};
use crate::error::{Error, Result};
use crate::logging;
use crate::ui::{FinalAction, Frontend, Progress};
use crate::validate;

/// Tools that must exist before a single byte is written. Checking up front
/// turns "missing package" from a half-installed disk into a clean refusal.
const REQUIRED_TOOLS: &[&str] = &[
    "sgdisk",
    "wipefs",
    "zpool",
    "zfs",
    "mkfs.vfat",
    "grub-install",
    "update-grub",
    "efibootmgr",
    "chroot",
    "mount",
    "umount",
    "mountpoint",
    "udevadm",
    "blkid",
    "blockdev",
    "cp",
    "chmod",
    "chpasswd",
    "useradd",
    "passwd",
    "visudo",
    "systemctl",
    "update-initramfs",
    "apt-get",
];

/// Named steps of the destructive phase, so progress reporting and the log
/// agree on what was happening when something failed.
const STEPS: &[&str] = &[
    "Partitioning disks",
    "Creating boot pool",
    "Creating root pool",
    "Creating datasets",
    "Creating EFI system partitions",
    "Copying the system",
    "Removing live-only components",
    "Configuring the target system",
    "Creating the nightshade account",
    "Preparing the boot environment",
    "Installing the bootloader",
    "Finalising",
];

pub fn run(frontend: &mut dyn Frontend) -> Result<FinalAction> {
    preflight()?;

    // --- 1. welcome -------------------------------------------------------
    frontend.welcome()?;

    // --- 2. disk selection ------------------------------------------------
    let disks = disk::enumerate()?;
    if disks.is_empty() {
        return Err(Error::env(
            "No installable disks were found.\n\
             Every block device was either virtual or the live medium itself.",
        ));
    }

    let selection = frontend.select_disks(&disks)?;
    let chosen: Vec<Disk> = selection.iter().map(|i| disks[*i].clone()).collect();

    if chosen.len() == 2 {
        let mismatch = disk::size_mismatch_ratio(&chosen[0], &chosen[1]);
        if mismatch > config::MIRROR_SIZE_TOLERANCE {
            // Allowed, not blocked: a mirror of mismatched disks is legal and
            // just wastes the difference. But it is far more often a misread of
            // the disk list than a deliberate choice.
            frontend.confirm_warning(&format!(
                "These disks differ in size by {:.0}%.\n\
                 \n\
                 {}  {}\n\
                 {}  {}\n\
                 \n\
                 The mirror will be sized to the smaller disk, so {} of the\n\
                 larger one will be unusable.",
                mismatch * 100.0,
                chosen[0].path.display(),
                chosen[0].size_human(),
                chosen[1].path.display(),
                chosen[1].size_human(),
                disk::human_bytes(
                    chosen[0].size_bytes.abs_diff(chosen[1].size_bytes)
                ),
            ))?;
        }
    }

    // --- 3. destruction confirmation --------------------------------------
    let refs: Vec<&Disk> = chosen.iter().collect();
    let layout = if chosen.len() >= 2 {
        Layout::Mirror
    } else {
        Layout::Single
    };
    frontend.confirm_destruction(&refs, layout)?;

    // --- 4. account -------------------------------------------------------
    let password = frontend.collect_password(validate::DEFAULT_USER)?;

    // --- 5. hostname ------------------------------------------------------
    let hostname = frontend.collect_hostname(validate::DEFAULT_HOSTNAME)?;

    let config = InstallConfig {
        disks: chosen,
        hostname,
        password,
    };

    // --- 6. summary -------------------------------------------------------
    frontend.confirm_summary(&config)?;

    // --- 7. install -------------------------------------------------------
    logging::step("beginning installation");
    log_plan(&config);

    let mut session = Session::new();
    let outcome = install(&config, frontend, &mut session);

    // Unwind mounts and pools either way. On success this leaves the pools
    // exported so the new system imports them cleanly on first boot; on
    // failure it leaves the machine in a state where re-running works.
    session.teardown();

    match outcome {
        Ok(()) => {
            logging::step("installation complete");
            // --- 8. done --------------------------------------------------
            frontend.finished(&logging::log_path())
        }
        Err(e) => {
            logging::error(format!("installation failed: {e}"));
            logging::block("failure detail", &e.detail());
            frontend.failed(&e, &logging::log_path());
            Err(e)
        }
    }
}

fn install(
    config: &InstallConfig,
    frontend: &mut dyn Frontend,
    session: &mut Session,
) -> Result<()> {
    let mut step = Stepper::new(frontend);

    step.begin("Partitioning disks");
    zfs::release_existing_pools(&mut step);
    for d in &config.disks {
        step.detail(format!("{} ({})", d.path.display(), d.size_human()));
        partition::prepare(d)?;
    }
    partition::settle(config)?;

    step.begin("Creating boot pool");
    zfs::create_bpool(config)?;
    session.pools.push(config::BPOOL.to_string());

    step.begin("Creating root pool");
    zfs::create_rpool(config)?;
    session.pools.push(config::RPOOL.to_string());

    step.begin("Creating datasets");
    zfs::create_datasets(&mut step)?;

    step.begin("Creating EFI system partitions");
    let esps = boot::format_esps(config, &mut step)?;
    for esp in &esps {
        session.mounts.push(esp.mountpoint.clone());
    }

    step.begin("Copying the system");
    let source = rootfs::source()?;
    step.detail(format!("from {}", source.display()));
    rootfs::copy_to_target(&source, Path::new(config::TARGET))?;

    step.begin("Removing live-only components");
    rootfs::strip_live_layer(&mut step)?;

    step.begin("Configuring the target system");
    session.bind_mounts = target::bind_pseudo_filesystems()?;
    target::write_fstab(&esps)?;
    target::set_hostname(config)?;
    target::enable_zfs_services(&mut step)?;

    step.begin("Creating the nightshade account");
    target::create_account(config)?;

    // Strict order: the cachefile has to exist before the initramfs is built,
    // because zfs-initramfs copies it into the image. That cached pool list is
    // how the initramfs knows which pool to import when there is not yet a
    // root filesystem to read the answer from. Build the initramfs first and
    // it ships without one.
    step.begin("Preparing the boot environment");
    zfs::write_cachefile(&mut step)?;
    target::regenerate_initramfs()?;

    step.begin("Installing the bootloader");
    boot::configure_grub()?;
    boot::install_to_esps(&esps, &mut step)?;
    if config.is_mirror() {
        boot::install_esp_sync(&esps, &mut step)?;
    }

    step.begin("Finalising");
    target::save_log()?;

    Ok(())
}

fn log_plan(config: &InstallConfig) {
    logging::info(format!("layout: {}", config.layout().label()));
    for d in &config.disks {
        logging::info(format!(
            "disk: {} ({}) via {}",
            d.path.display(),
            d.size_human(),
            d.stable_path.display()
        ));
    }
    logging::info(format!("hostname: {}", config.hostname));
}

/// Refuse to start if the environment cannot support an install.
fn preflight() -> Result<()> {
    logging::step("preflight");

    if !is_root() {
        return Err(Error::env(
            "The installer must run as root. Try: sudo nightshade-installer",
        ));
    }

    disk::require_tools(REQUIRED_TOOLS)?;

    // The ZFS module is built into the image, but if it is missing then every
    // zpool command later would fail one at a time with confusing errors.
    if !Cmd::new("modprobe").arg("zfs").succeeds() {
        return Err(Error::env(
            "The ZFS kernel module could not be loaded.\n\
             This image is broken; `modprobe zfs` failed.",
        ));
    }

    if !Path::new("/sys/firmware/efi").exists() {
        return Err(Error::env(
            "This machine was not booted in UEFI mode.\n\
             Nightshade is UEFI-only: reboot the installer in UEFI mode\n\
             (disable legacy/CSM boot in firmware setup) and try again.",
        ));
    }

    logging::info("preflight passed");
    Ok(())
}

fn is_root() -> bool {
    // No libc dependency: /proc/self/status reports the effective uid.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(2).map(|s| s.to_string()))
        })
        .map(|euid| euid == "0")
        .unwrap_or(false)
}

/// Progress reporter that knows the step numbering.
pub struct Stepper<'a> {
    frontend: &'a mut dyn Frontend,
    index: usize,
}

impl<'a> Stepper<'a> {
    fn new(frontend: &'a mut dyn Frontend) -> Self {
        Stepper { frontend, index: 0 }
    }

    fn begin(&mut self, name: &str) {
        self.index += 1;
        logging::step(name);
        self.frontend.progress(Progress::Step {
            index: self.index,
            total: STEPS.len(),
            name: name.to_string(),
        });
    }

    pub fn detail(&mut self, detail: impl Into<String>) {
        let detail = detail.into();
        logging::info(&detail);
        self.frontend.progress(Progress::Detail(detail));
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        logging::warn(&warning);
        self.frontend.progress(Progress::Warning(warning));
    }
}

/// Everything that has to be undone before the installer exits.
#[derive(Default)]
struct Session {
    /// Mount points, unmounted in reverse order.
    mounts: Vec<String>,
    bind_mounts: Vec<String>,
    /// Imported pools, exported last.
    pools: Vec<String>,
}

impl Session {
    fn new() -> Self {
        Session::default()
    }

    /// Unwind in strict reverse dependency order: binds, then filesystems,
    /// then the datasets underneath them, then the pools.
    ///
    /// Every step is lenient. Teardown runs on the failure path too, where
    /// some of these were never created, and a noisy unmount error must not
    /// bury the real cause of the failure.
    fn teardown(&mut self) {
        logging::step("teardown");

        // -R, not -l. These were made with --rbind, so each one is a tree with
        // nested mounts inside it (/dev/pts, /dev/shm, /run/...). A lazy
        // unmount detaches only the top of that tree and returns success; the
        // submounts stay, they live inside rpool's filesystem, and the pool
        // then refuses to export because it is still busy. -lR is the fallback
        // for the case where something genuinely still holds a file open.
        for m in self.bind_mounts.iter().rev() {
            let ok = Cmd::new("umount").arg("-R").arg(m.clone()).succeeds();
            if !ok {
                let _ = Cmd::new("umount").arg("-lR").arg(m.clone()).run_lenient();
            }
        }
        for m in self.mounts.iter().rev() {
            let _ = Cmd::new("umount").arg(m.clone()).run_lenient();
        }

        // Unmount every dataset before exporting, otherwise the export fails
        // and the pool stays imported into the reboot.
        let _ = Cmd::new("zfs").arg("unmount").arg("-a").run_lenient();

        // Forward order, NOT reverse: bpool is pushed first and must also be
        // exported first, because its mountpoint (/boot) sits inside rpool's
        // altroot tree. Exporting rpool while bpool is still imported inside it
        // fails with "pool is busy", and a pool that was never cleanly exported
        // needs a forced import on the next boot -- which zfs-import-cache does
        // not do, so the new system would fail to find its root.
        let pools = self.pools.clone();
        for pool in &pools {
            if export_pool(pool, false) {
                continue;
            }
            // One retry: an unmount that has only just completed can leave the
            // pool briefly busy.
            std::thread::sleep(std::time::Duration::from_millis(500));
            if export_pool(pool, false) {
                continue;
            }

            // Record what is still holding it. "pool is busy" on its own is not
            // enough to diagnose this from a log after the fact.
            log_remaining_mounts();

            // Then force it. By this point nothing of ours is mounted from the
            // pool, so -f only tells ZFS to stop waiting on a reference it
            // still thinks is outstanding. The alternative is leaving the pool
            // imported, which makes the next boot force-IMPORT instead -- and a
            // forced import is the direction that actually risks data, because
            // it overrides the check for another host having the pool open.
            if export_pool(pool, true) {
                logging::warn(format!("{pool} needed a forced export"));
                continue;
            }

            logging::warn(format!(
                "could not export {pool}; the new system may need `zpool import -f {pool}`"
            ));
        }

        self.bind_mounts.clear();
        self.mounts.clear();
        self.pools.clear();
    }
}

/// Log anything still mounted under the install target.
fn log_remaining_mounts() {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return;
    };
    let still: Vec<&str> = mounts
        .lines()
        .filter(|l| l.split_whitespace().nth(1).is_some_and(|m| m.starts_with(config::TARGET)))
        .collect();
    if still.is_empty() {
        logging::warn("nothing is mounted under the target; the pool is held by something else");
    } else {
        logging::block("still mounted under the target", &still.join("\n"));
    }
}

fn export_pool(pool: &str, force: bool) -> bool {
    let mut cmd = Cmd::new("zpool").arg("export");
    if force {
        cmd = cmd.arg("-f");
    }
    match cmd.arg(pool).run_lenient() {
        Ok(o) if o.ok() => {
            logging::info(format!("exported {pool}"));
            true
        }
        _ => false,
    }
}
