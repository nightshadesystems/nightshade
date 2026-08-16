//! EFI system partitions, GRUB, and keeping a mirror's two ESPs in step.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::Stepper;
use crate::cmd::Cmd;
use crate::config::{InstallConfig, PART_ESP, ROOT_DATASET, TARGET};

use crate::error::{Error, IoContext, Result};
use crate::logging;

/// Where the image keeps the assets built by hook 0800.
const THEME_SOURCE: &str = "/usr/share/nightshade/grub-theme";
const FONTS_SOURCE: &str = "/usr/share/nightshade/grub-fonts";
const DEFAULT_GRUB_TEMPLATE: &str = "/usr/share/nightshade/default-grub.in";

/// Theme location on the installed system.
///
/// grub-mkconfig emits `loadfont` for every .pf2 it finds in the theme's own
/// directory, so the fonts live in there rather than in /boot/grub/fonts.
/// Fonts anywhere else are simply never loaded and the menu silently falls
/// back to the built-in bitmap font.
const THEME_DIR: &str = "/boot/grub/themes/nightshade";

pub struct Esp {
    pub disk_index: usize,
    /// Kernel-name partition device, e.g. /dev/vda1.
    pub device: PathBuf,
    /// Parent disk, which is what efibootmgr wants.
    pub disk: PathBuf,
    /// Absolute path on the live system, e.g. /mnt/nightshade/boot/efi.
    pub mountpoint: String,
    /// Path as the installed system sees it, e.g. /boot/efi.
    pub target_path: String,
    pub uuid: String,
}

/// Format and mount an ESP on every selected disk.
pub fn format_esps(config: &InstallConfig, step: &mut Stepper) -> Result<Vec<Esp>> {
    let mut esps = Vec::new();

    for (index, disk) in config.disks.iter().enumerate() {
        let device = disk.kernel_partition(PART_ESP);
        let dev = device.to_string_lossy().into_owned();

        // FAT32 with a 512-byte logical sector size. The UEFI spec asks for
        // FAT32 on a fixed disk, and being explicit avoids mkfs picking FAT16
        // on the smaller end of the size range.
        let label = format!("NSEFI{index}");
        Cmd::new("mkfs.vfat")
            .arg("-F")
            .arg("32")
            .arg("-s")
            .arg("1")
            .arg("-n")
            .arg(&label)
            .arg(&dev)
            .run()?;

        let mountpoint = config.esp_mountpoint(index);
        fs::create_dir_all(&mountpoint).ctx(format!("creating {mountpoint}"))?;
        Cmd::new("mount").arg(&dev).arg(&mountpoint).run()?;

        let uuid = fs_uuid(&device)?;
        step.detail(format!("{dev} -> {mountpoint} (UUID {uuid})"));

        let target_path = if index == 0 {
            "/boot/efi".to_string()
        } else {
            format!("/boot/efi{}", index + 1)
        };

        esps.push(Esp {
            disk_index: index,
            device,
            disk: disk.path.clone(),
            mountpoint,
            target_path,
            uuid,
        });
    }

    Ok(esps)
}

fn fs_uuid(device: &Path) -> Result<String> {
    let out = Cmd::new("blkid")
        .arg("-s")
        .arg("UUID")
        .arg("-o")
        .arg("value")
        .arg(device.to_string_lossy().into_owned())
        .run()?;
    let uuid = out.trimmed().to_string();
    if uuid.is_empty() {
        return Err(Error::env(format!(
            "blkid reported no UUID for {}",
            device.display()
        )));
    }
    Ok(uuid)
}

/// Write /etc/default/grub and put the Nightshade theme where GRUB will find it.
pub fn configure_grub() -> Result<()> {
    // The template travelled with the image, so the installed menu is styled
    // from the same source as the ISO menu.
    let template_path = format!("{TARGET}{DEFAULT_GRUB_TEMPLATE}");
    let template = fs::read_to_string(&template_path)
        .ctx(format!("reading the GRUB template at {template_path}"))?;

    let version = os_release_field("VERSION_ID").unwrap_or_else(|| "0.1.0".to_string());
    let rendered = template
        .replace("@ROOT_DATASET@", ROOT_DATASET)
        .replace("@VERSION@", &version);

    let default_grub = format!("{TARGET}/etc/default/grub");
    fs::write(&default_grub, &rendered).ctx(format!("writing {default_grub}"))?;
    logging::info(format!("wrote {default_grub}"));

    // Theme and fonts.
    let theme_src = format!("{TARGET}{THEME_SOURCE}");
    let theme_dst = format!("{TARGET}{THEME_DIR}");
    if !Path::new(&theme_src).is_dir() {
        return Err(Error::env(format!(
            "the image is missing its GRUB theme at {THEME_SOURCE}"
        )));
    }
    fs::create_dir_all(&theme_dst).ctx(format!("creating {theme_dst}"))?;
    Cmd::new("cp")
        .arg("-a")
        .arg(format!("{theme_src}/."))
        .arg(&theme_dst)
        .run()?;

    let fonts_src = format!("{TARGET}{FONTS_SOURCE}");
    if Path::new(&fonts_src).is_dir() {
        // Into the theme directory; see the note on THEME_DIR.
        Cmd::new("cp")
            .arg("-a")
            .arg(format!("{fonts_src}/."))
            .arg(&theme_dst)
            .run()?;
    }

    if !Path::new(&format!("{theme_dst}/theme.txt")).exists() {
        return Err(Error::env(
            "the GRUB theme did not land in /boot/grub/themes/nightshade",
        ));
    }

    Ok(())
}

/// Install GRUB onto every ESP and generate the menu.
pub fn install_to_esps(esps: &[Esp], step: &mut Stepper) -> Result<()> {
    for esp in esps {
        step.detail(format!(
            "GRUB -> {} (disk {})",
            esp.target_path,
            esp.disk.display()
        ));

        // Pass 1: a named installation.
        //
        // BOTH ESPs use the same bootloader-id. A per-disk id seems tidier
        // until nightshade-sync-esp mirrors ESP 1 onto ESP 2 and replaces
        // EFI/Nightshade-disk2/ with EFI/Nightshade/ -- at which point disk 2's
        // NVRAM entry points at a path that no longer exists and the firmware
        // reports "Not Found" on every boot. The two entries are distinguished
        // by the partition they live on, not by their path.
        let mut install = Cmd::new("grub-install")
            .in_chroot(TARGET)
            .arg("--target=x86_64-efi")
            .arg(format!("--efi-directory={}", esp.target_path))
            .arg("--boot-directory=/boot")
            .arg("--bootloader-id=Nightshade")
            .arg("--recheck");

        if esp.disk_index > 0 {
            // grub-install deletes any NVRAM entry matching its bootloader-id
            // before adding its own, so a second run with the same id would
            // remove the first disk's entry. Skip NVRAM here; the entry for
            // this disk is added explicitly further down.
            install = install.arg("--no-nvram");
        }
        install.run()?;

        // Pass 2: the removable-media fallback path, EFI/BOOT/BOOTX64.EFI.
        //
        // This is what makes a degraded mirror actually boot. If the disk whose
        // NVRAM entry the firmware prefers is gone, most firmware falls back to
        // the fallback path on the next device it finds -- but only if one is
        // there. Without this, pulling disk 0 leaves a machine that has a
        // perfectly good bootloader on disk 1 and no way to reach it.
        Cmd::new("grub-install")
            .in_chroot(TARGET)
            .arg("--target=x86_64-efi")
            .arg(format!("--efi-directory={}", esp.target_path))
            .arg("--boot-directory=/boot")
            .arg("--removable")
            .arg("--recheck")
            .run()?;
    }

    // One NVRAM entry per additional disk, pointing at that disk's own ESP but
    // at the same loader path the sync unit keeps in place there.
    for esp in esps.iter().skip(1) {
        let label = format!("Nightshade (disk {})", esp.disk_index + 1);
        step.detail(format!("firmware boot entry: {label}"));
        let out = Cmd::new("efibootmgr")
            .in_chroot(TARGET)
            .arg("--create")
            .arg("--disk")
            .arg(esp.disk.to_string_lossy().into_owned())
            .arg("--part")
            .arg(PART_ESP.to_string())
            .arg("--label")
            .arg(&label)
            .arg("--loader")
            .arg(r"\EFI\Nightshade\grubx64.efi")
            .run_lenient()?;
        if !out.ok() {
            // Not fatal. The removable fallback below still boots this disk;
            // the machine just will not have a named entry for it.
            step.warn(format!(
                "could not add a firmware boot entry for disk {}; \
                 it will still boot via the fallback path",
                esp.disk_index + 1
            ));
        }
    }

    // Generate the menu once; it lives on bpool and is shared by both ESPs.
    step.detail("generating the boot menu");
    Cmd::new("update-grub").in_chroot(TARGET).run()?;

    let cfg = format!("{TARGET}/boot/grub/grub.cfg");
    let generated = fs::read_to_string(&cfg).ctx(format!("reading {cfg}"))?;
    // If grub-probe failed to identify the ZFS root, the menu will boot into a
    // kernel panic. Catch it here, where it is still fixable.
    if !generated.contains(ROOT_DATASET) {
        return Err(Error::env(format!(
            "the generated boot menu does not reference {ROOT_DATASET}.\n\
             grub-probe did not recognise the ZFS root; the system would not boot."
        )));
    }
    if !generated.contains("themes/nightshade") {
        step.warn("the generated menu has no theme; it will boot but look unbranded");
    }
    step.detail(format!("menu generated ({} bytes)", generated.len()));

    // Record the firmware's boot entries. When a machine comes up on the wrong
    // disk, or refuses to boot degraded, this is the first thing worth reading.
    if let Ok(out) = Cmd::new("efibootmgr").arg("-v").run_lenient() {
        logging::block("efibootmgr -v", &out.stdout);
    }

    Ok(())
}

/// Install the ESP mirroring hook and its systemd units.
///
/// grub-install and every future kernel update write to /boot/efi only. Without
/// this the second disk's ESP is a snapshot of installation day, and the first
/// time it is actually needed -- when the other disk has failed -- it boots a
/// bootloader that no longer matches the system.
pub fn install_esp_sync(esps: &[Esp], step: &mut Stepper) -> Result<()> {
    let Some(secondary) = esps.get(1) else {
        return Ok(());
    };

    let script = format!(
        r#"#!/bin/sh
# Mirror the primary EFI system partition onto the secondary.
#
# Installed by nightshade-install for mirrored (two-disk) systems. Triggered by
# nightshade-sync-esp.path whenever /boot/efi changes, and once at boot to catch
# drift from any period when the second disk was absent.
set -eu

PRIMARY=/boot/efi
SECONDARY={secondary}

log() {{ echo "nightshade-sync-esp: $*"; }}

if ! mountpoint -q "$PRIMARY"; then
    log "$PRIMARY is not mounted; nothing to mirror"
    exit 0
fi

# The secondary is noauto in fstab, so mount it for the duration. If its disk
# is missing -- exactly the degraded case this exists for -- exit quietly
# rather than failing a boot that is otherwise fine.
UNMOUNT=no
if ! mountpoint -q "$SECONDARY"; then
    if ! mount "$SECONDARY" 2>/dev/null; then
        log "cannot mount $SECONDARY (disk missing?); skipping"
        exit 0
    fi
    UNMOUNT=yes
fi

cleanup() {{
    [ "$UNMOUNT" = yes ] && umount "$SECONDARY" 2>/dev/null || true
}}
trap cleanup EXIT

log "mirroring $PRIMARY/EFI to $SECONDARY/EFI"
rm -rf "$SECONDARY/EFI"
cp -a "$PRIMARY/EFI" "$SECONDARY/"
sync
log "done"
"#,
        secondary = secondary.target_path
    );

    write_target_file("/usr/local/sbin/nightshade-sync-esp", &script, 0o755)?;

    let service = "\
[Unit]
Description=Mirror the primary ESP onto the second disk
Documentation=man:nightshade-sync-esp(8)
After=local-fs.target
ConditionPathIsMountPoint=/boot/efi

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/nightshade-sync-esp

[Install]
WantedBy=multi-user.target
";
    write_target_file("/etc/systemd/system/nightshade-sync-esp.service", service, 0o644)?;

    // PathChanged fires on close-after-write, which is what grub-install does
    // to the binaries. PathModified on the directory catches files being added
    // or removed, which is what a kernel package update does.
    let path_unit = "\
[Unit]
Description=Watch the primary ESP for changes

[Path]
PathModified=/boot/efi/EFI
PathChanged=/boot/efi/EFI/BOOT/BOOTX64.EFI
PathChanged=/boot/efi/EFI/Nightshade/grubx64.efi
Unit=nightshade-sync-esp.service

[Install]
WantedBy=multi-user.target
";
    write_target_file("/etc/systemd/system/nightshade-sync-esp.path", path_unit, 0o644)?;

    for unit in ["nightshade-sync-esp.service", "nightshade-sync-esp.path"] {
        let out = Cmd::new("systemctl")
            .in_chroot(TARGET)
            .arg("enable")
            .arg(unit)
            .run_lenient()?;
        if !out.ok() {
            step.warn(format!("could not enable {unit}; see the log"));
        }
    }

    // Run it once now so the second ESP is correct before the first boot,
    // rather than only after something happens to change the first one.
    let out = Cmd::new("/usr/local/sbin/nightshade-sync-esp")
        .in_chroot(TARGET)
        .run_lenient()?;
    if out.ok() {
        step.detail("second ESP synchronised");
    } else {
        step.warn("initial ESP sync reported an error; see the log");
    }

    Ok(())
}

fn os_release_field(key: &str) -> Option<String> {
    let text = fs::read_to_string(format!("{TARGET}/etc/os-release")).ok()?;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(&format!("{key}=")) {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

fn write_target_file(relative: &str, contents: &str, mode: u32) -> Result<()> {
    let path = format!("{TARGET}{relative}");
    let path = Path::new(&path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
    }
    fs::write(path, contents).ctx(format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .ctx(format!("setting mode on {}", path.display()))?;
    logging::info(format!("wrote {} (mode {:o})", path.display(), mode));
    Ok(())
}
