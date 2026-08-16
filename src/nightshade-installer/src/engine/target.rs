//! Configuring the copied system in place, through a chroot.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::Stepper;
use super::boot::Esp;
use crate::cmd::Cmd;
use crate::config::{InstallConfig, TARGET};
use crate::error::{Error, IoContext, Result};
use crate::logging;
use crate::secret::SecretString;
use crate::validate::DEFAULT_USER;

/// Bind the host's pseudo-filesystems into the target.
///
/// grub-install needs /dev and /sys to work out what it is installing onto,
/// efibootmgr needs /sys/firmware/efi/efivars, and maintainer scripts need
/// /proc. Recursive binds carry the nested mounts (devpts, efivars) with them.
pub fn bind_pseudo_filesystems() -> Result<Vec<String>> {
    let mut mounted = Vec::new();

    for (source, name) in [("/dev", "dev"), ("/sys", "sys"), ("/run", "run")] {
        let target = format!("{TARGET}/{name}");
        fs::create_dir_all(&target).ctx(format!("creating {target}"))?;
        Cmd::new("mount").arg("--rbind").arg(source).arg(&target).run()?;
        // rslave stops unmounts inside the chroot from propagating back out
        // and tearing down the live system's own /dev.
        Cmd::new("mount").arg("--make-rslave").arg(&target).run()?;
        mounted.push(target);
    }

    let proc = format!("{TARGET}/proc");
    fs::create_dir_all(&proc).ctx(format!("creating {proc}"))?;
    Cmd::new("mount").arg("-t").arg("proc").arg("proc").arg(&proc).run()?;
    mounted.push(proc);

    Ok(mounted)
}

/// /etc/fstab carries the ESPs and nothing else.
///
/// Every ZFS dataset mounts from its own `mountpoint` property via the zfs
/// systemd units, so listing them here would be a second, divergent source of
/// truth for the same thing.
pub fn write_fstab(esps: &[Esp]) -> Result<()> {
    let mut fstab = String::from(
        "# /etc/fstab -- Nightshade\n\
         #\n\
         # ZFS datasets are not listed here. They are mounted from their own\n\
         # mountpoint properties by zfs-mount.service after zfs-import-cache\n\
         # imports the pools.\n\n",
    );

    for (i, esp) in esps.iter().enumerate() {
        if i == 0 {
            fstab.push_str(&format!("# EFI system partition ({}).\n", esp.device.display()));
            // nofail matters: on a mirror whose first disk has died, a missing
            // ESP must not drop the machine into emergency mode when it could
            // have booted perfectly well off the survivor.
            //
            // device-timeout matters just as much. nofail alone still makes
            // systemd wait out the full 90s default for a device that is never
            // coming back, so a dead disk silently costs a minute and a half on
            // every single boot. Five seconds is plenty for a device that is
            // actually present.
            fstab.push_str(&format!(
                "UUID={}  /boot/efi   vfat  \
                 umask=0077,shortname=mixed,nofail,x-systemd.device-timeout=5s  0  1\n\n",
                esp.uuid
            ));
        } else {
            fstab.push_str(
                "# Mirror of the ESP above, kept current by nightshade-sync-esp.\n\
                 # noauto: two mounted ESPs invite writing to whichever one\n\
                 # happens to be mounted; the sync unit mounts it when needed.\n",
            );
            fstab.push_str(&format!(
                "UUID={}  /boot/efi2  vfat  \
                 umask=0077,shortname=mixed,noauto,nofail,x-systemd.device-timeout=5s  0  0\n",
                esp.uuid
            ));
        }
    }

    write_target_file("/etc/fstab", &fstab, 0o644)
}

pub fn set_hostname(config: &InstallConfig) -> Result<()> {
    let host = &config.hostname;
    write_target_file("/etc/hostname", &format!("{host}\n"), 0o644)?;

    // The 127.0.1.1 line is what makes `sudo` and `hostname -f` resolve without
    // a network. Debian sets it the same way.
    let short = host.split('.').next().unwrap_or(host);
    let hosts = format!(
        "127.0.0.1\tlocalhost\n\
         127.0.1.1\t{host}\t{short}\n\
         \n\
         ::1\t\tlocalhost ip6-localhost ip6-loopback\n\
         ff02::1\t\tip6-allnodes\n\
         ff02::2\t\tip6-allrouters\n"
    );
    write_target_file("/etc/hosts", &hosts, 0o644)
}

pub fn enable_zfs_services(step: &mut Stepper) -> Result<()> {
    // zfs-import-cache imports exactly the pools listed in the cachefile;
    // zfs-mount mounts their datasets; the targets order everything else
    // against them.
    //
    // zfs-zed is deliberately absent: it lives in a separate zfs-zed package
    // that is not in the image manifest, so enabling it would only ever log a
    // failure. Fault-event handling is a phase-2 concern along with the rest of
    // the notification story.
    let units = [
        "zfs-import-cache.service",
        "zfs-mount.service",
        "zfs-import.target",
        "zfs.target",
    ];

    for unit in units {
        let out = Cmd::new("systemctl")
            .in_chroot(TARGET)
            .arg("enable")
            .arg(unit)
            .run_lenient()?;
        if !out.ok() {
            step.warn(format!("could not enable {unit}; see the log"));
        }
    }

    // zfs-mount-generator would double-mount what zfs-mount.service already
    // handles; Debian ships it disabled and we keep it that way.
    step.detail("zfs import and mount units enabled");
    Ok(())
}

/// Create the nightshade account, lock root, grant sudo.
pub fn create_account(config: &InstallConfig) -> Result<()> {
    logging::info(format!("creating account {DEFAULT_USER}"));

    // `ns` is the login shell when the image has a configuration system, and
    // /bin/bash when it does not. Checked rather than assumed: setting a login
    // shell that is not there produces an account that cannot be logged into,
    // and this is the only account on the box.
    //
    // The operator is not shut out of a real shell by this. `shell` from
    // operational mode drops to bash as their own uid, and root still has
    // /bin/bash -- it is just that reaching one is a deliberate, audited step
    // rather than what happens by default.
    let ns = std::path::Path::new(TARGET).join("usr/bin/ns");
    let shell = if ns.exists() { "/usr/bin/ns" } else { "/bin/bash" };
    if shell == "/bin/bash" {
        logging::info("no ns binary in the image; the account gets /bin/bash");
    }

    // sudo to administer the box, nightshade-admin to talk to configd. The
    // group only exists in an image that has a configuration system.
    let groups = if ns.exists() {
        "sudo,nightshade-admin"
    } else {
        "sudo"
    };

    Cmd::new("useradd")
        .in_chroot(TARGET)
        .arg("--create-home")
        .arg("--shell")
        .arg(shell)
        .arg("--groups")
        .arg(groups)
        .arg("--comment")
        .arg("Nightshade administrator")
        .arg(DEFAULT_USER)
        .run()?;

    // The password goes in on stdin and nowhere else. Putting it in argv would
    // expose it to every process on the machine through /proc/<pid>/cmdline,
    // and a temp file would outlive the write.
    let line = SecretString::new(format!(
        "{DEFAULT_USER}:{}\n",
        config.password.expose()
    ));
    Cmd::new("chpasswd").in_chroot(TARGET).stdin_secret(line).run()?;

    // Root has no password and cannot be logged into; administration is sudo
    // from the nightshade account.
    Cmd::new("passwd").in_chroot(TARGET).arg("--lock").arg("root").run()?;

    // 0440 is what sudo requires; anything more permissive and sudo refuses to
    // read the file at all, which locks the operator out of their own machine.
    write_target_file(
        "/etc/sudoers.d/nightshade",
        &format!(
            "# Nightshade administrator. Password required: this is the only\n\
             # account on the box and NOPASSWD would make a stolen session total.\n\
             {DEFAULT_USER} ALL=(ALL:ALL) ALL\n"
        ),
        0o440,
    )?;

    // Validate before we walk away from it. A malformed sudoers drop-in makes
    // sudo refuse everything, and root is locked, so the machine would be
    // unadministerable.
    let check = Cmd::new("visudo")
        .in_chroot(TARGET)
        .arg("--check")
        .arg("--file=/etc/sudoers.d/nightshade")
        .run_lenient()?;
    if !check.ok() {
        let _ = fs::remove_file(format!("{TARGET}/etc/sudoers.d/nightshade"));
        return Err(Error::env(format!(
            "the generated sudoers file was rejected by visudo:\n{}",
            check.stderr.trim()
        )));
    }

    // And check the whole configuration, in case the drop-in is fine alone but
    // conflicts with what is already there.
    Cmd::new("visudo").in_chroot(TARGET).arg("--check").run()?;

    logging::info("account created, root locked, sudo validated");
    Ok(())
}

/// Rebuild the initramfs inside the target.
///
/// Must run after the pool cache is written: zfs-initramfs copies
/// /etc/zfs/zpool.cache into the image, and that is how the initramfs knows
/// which pool to import before there is a root filesystem to read it from.
pub fn regenerate_initramfs() -> Result<()> {
    Cmd::new("update-initramfs")
        .in_chroot(TARGET)
        .arg("-u")
        .arg("-k")
        .arg("all")
        .run()?;
    Ok(())
}

/// Copy the install log into the new system.
pub fn save_log() -> Result<()> {
    let dest = format!("{TARGET}/var/log/nightshade-install.log");
    if let Err(e) = logging::copy_to(Path::new(&dest)) {
        // Never fail an otherwise good install over a log copy.
        logging::warn(format!("could not copy the install log to {dest}: {e}"));
    }
    Ok(())
}

/// Write a file inside the target, creating parents, with an explicit mode.
fn write_target_file(relative: &str, contents: &str, mode: u32) -> Result<()> {
    let path = format!("{TARGET}{relative}");
    let path = Path::new(&path);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    fs::write(path, contents).ctx(format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .ctx(format!("setting mode {mode:o} on {}", path.display()))?;

    logging::info(format!("wrote {} (mode {:o})", path.display(), mode));
    Ok(())
}
