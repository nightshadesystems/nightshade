//! Getting the operating system onto the target.
//!
//! The target rootfs is a copy of the squashfs the live system is running
//! from, not a second debootstrap. One source of truth for the package set:
//! whatever was tested when the ISO booted is exactly what lands on disk, and
//! an install cannot silently pull a newer package than the image was built
//! and gated against. It is also much faster and needs no network.

use std::fs;
use std::path::{Path, PathBuf};

use super::Stepper;
use crate::cmd::Cmd;
use crate::config::TARGET;
use crate::error::{Error, IoContext, Result};
use crate::logging;

/// Manifest written by build hook 0600 listing everything live-only.
const LIVE_MANIFEST: &str = "/usr/share/nightshade/live-manifest";

/// Locate the read-only squashfs the live system booted from.
///
/// Deliberately the squashfs mount and not `/`: `/` is an overlay, and copying
/// it would drag along every change the live session has made, including the
/// installer's own logs and any host keys generated at boot.
pub fn source() -> Result<PathBuf> {
    let mounts = fs::read_to_string("/proc/mounts").ctx("reading /proc/mounts")?;

    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(target), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if fstype != "squashfs" {
            continue;
        }
        let path = PathBuf::from(target);
        // Sanity-check that it really is a root filesystem before copying it.
        if path.join("usr/bin").is_dir() && path.join("etc/os-release").exists() {
            logging::info(format!("install source: {}", path.display()));
            return Ok(path);
        }
        logging::warn(format!(
            "ignoring squashfs at {} : does not look like a root filesystem",
            path.display()
        ));
    }

    Err(Error::env(
        "Could not find the live squashfs to install from.\n\
         The installer must run from the Nightshade live medium.",
    ))
}

pub fn copy_to_target(source: &Path, target: &Path) -> Result<()> {
    // cp -a: recursive, preserves ownership, modes, timestamps, symlinks,
    // device nodes and hard links. The trailing /. copies the directory's
    // contents (including dotfiles) rather than the directory itself.
    Cmd::new("cp")
        .arg("-a")
        .arg(format!("{}/.", source.display()))
        .arg(target.to_string_lossy().into_owned())
        .run()?;

    // Prove the copy actually produced a system rather than failing partway
    // with a zero exit somewhere in the middle.
    for required in ["usr/bin", "etc/os-release", "sbin/init", "boot"] {
        let p = target.join(required);
        if !p.exists() {
            return Err(Error::env(format!(
                "the copied system is missing {required}; the copy did not complete"
            )));
        }
    }

    Ok(())
}

/// Remove everything that only made sense while running from the ISO.
///
/// Driven from the manifest the build wrote, so "what is live-only" is defined
/// in exactly one place -- the hook that created those files -- instead of
/// being duplicated here and drifting.
pub fn strip_live_layer(step: &mut Stepper) -> Result<()> {
    let manifest_path = format!("{TARGET}{LIVE_MANIFEST}");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            // Not fatal: an image without a manifest is one built before this
            // existed. Say so loudly rather than silently shipping live-boot.
            step.warn(format!(
                "no live manifest at {LIVE_MANIFEST} ({e}); live packages will remain"
            ));
            return Ok(());
        }
    };

    let mut packages = Vec::new();
    let mut paths = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once(char::is_whitespace) {
            Some(("pkg", value)) => packages.push(value.trim().to_string()),
            Some(("path", value)) => paths.push(value.trim().to_string()),
            _ => step.warn(format!("ignoring unparseable live-manifest line: {line}")),
        }
    }

    if !packages.is_empty() {
        step.detail(format!("purging {} live packages", packages.len()));
        // Lenient: a package listed but not installed is not a failure, and
        // apt returns non-zero for the whole transaction in that case.
        let out = Cmd::new("apt-get")
            .in_chroot(TARGET)
            .arg("purge")
            .arg("--yes")
            .arg("--autoremove")
            .args(packages.clone())
            .run_lenient()?;
        if !out.ok() {
            step.warn("apt-get purge of live packages reported an error; see the log");
        }
    }

    for path in &paths {
        let full = format!("{TARGET}{path}");
        let full = Path::new(&full);
        if full.is_dir() {
            let _ = fs::remove_dir_all(full);
        } else if full.exists() || full.is_symlink() {
            let _ = fs::remove_file(full);
        }
    }
    step.detail(format!("removed {} live-only paths", paths.len()));

    // The installer binary and its unit are gone; make sure nothing still
    // points at them.
    let leftover = format!("{TARGET}/etc/systemd/system/multi-user.target.wants/nightshade-installer.service");
    let _ = fs::remove_file(&leftover);

    Ok(())
}
