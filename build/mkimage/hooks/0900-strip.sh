#!/bin/sh
# Final pass: purge the build toolchain, restore the ZFS module, strip the tree.
#
# Runs last because everything before it needs a compiler, fonts or mtools.
. /run/nightshade-build/hooks/lib.sh

KVER="$(cat "$STAGING/stash/kver")"

# --- protect the manifest from autoremove --------------------------------
# autoremove decides what is an orphan from the manual/auto flags. Anything in
# our manifests is intentional even if it happens to also be a dependency of
# something we are about to purge, so pin the flags first. Without this,
# purging build-essential can quietly take parts of the image with it.
log "marking manifest packages as manually installed"
# shellcheck disable=SC2046
apt-mark manual $(read_manifest "$STAGING/packages.list") \
                $(read_manifest "$STAGING/packages-live.list") >/dev/null

# --- satisfy zfs-initramfs before removing zfs-dkms -----------------------
# Must happen BEFORE the purge. zfs-initramfs depends on "zfs-modules |
# zfs-dkms"; installing our prebuilt-modules package first means apt sees the
# dependency still satisfied when zfs-dkms goes, and leaves zfs-initramfs alone.
# Do this after the purge instead and apt has already taken zfs-initramfs with
# it, which produces an image that cannot mount its own root pool.
log "installing nightshade-zfs-modules (Provides: zfs-modules)"
dpkg -i "$STAGING/nightshade-zfs-modules.deb"

# --- purge the toolchain --------------------------------------------------
# Derived from packages-build.list rather than repeated here. A hardcoded copy
# drifts the moment someone adds a build-only package, and the symptom is a
# build tool silently shipping in the image.
BUILD_ONLY=$(read_manifest "$STAGING/packages-build.list" | tr '\n' ' ')
# zfs-dkms and dkms are in packages.list (they are needed to produce the
# module) but must not survive into the image; hook 0300 stashed their output.
PURGE="$BUILD_ONLY zfs-dkms dkms"
# Enumerate the header packages that are actually installed rather than handing
# apt a glob, which it would interpret as a regex against every package name.
HEADERS=$(dpkg-query -W -f='${Package} ${db:Status-Status}\n' 'linux-headers-*' 2>/dev/null \
          | awk '$2 == "installed" { print $1 }')

log "purging build toolchain: $PURGE $HEADERS"
# shellcheck disable=SC2086
apt-get purge --yes $PURGE $HEADERS
apt-get autoremove --purge --yes

# --- restore the built module --------------------------------------------
# `apt-get purge zfs-dkms` ran `dkms remove`, which deleted the module we built
# in hook 0300. Put it back as a plain file: the image keeps a working ZFS
# driver without keeping the machinery that produced it.
# `apt-get purge zfs-dkms` ran `dkms remove`, which deletes the module files
# even though nightshade-zfs-modules now owns them as far as dpkg is concerned.
# Put them back so the package's contents and the filesystem agree.
log "restoring stashed ZFS module for $KVER"
mkdir -p "/lib/modules/$KVER/updates"
rm -rf "/lib/modules/$KVER/updates/dkms"
cp -a "$STAGING/stash/modules/dkms" "/lib/modules/$KVER/updates/dkms"
depmod -a "$KVER"

# zfs-initramfs must have come through the purge; everything about booting
# depends on it.
if [ ! -f /usr/share/initramfs-tools/hooks/zfs ]; then
    die "zfs-initramfs did not survive the purge.
  Its 'zfs-modules | zfs-dkms' dependency was not satisfied when zfs-dkms was
  removed, so apt took it too. The image would boot to an initramfs prompt."
fi
log "zfs-initramfs survived the purge"

log "gate: modinfo zfs after purge"
modinfo -k "$KVER" zfs >/dev/null 2>&1 \
    || die "ZFS module did not survive the toolchain purge"
# The compiler must be gone; an appliance that can build kernel modules is a
# much more interesting target than one that cannot.
if command -v gcc >/dev/null 2>&1; then
    die "gcc is still present after the purge"
fi

# Prove the purge actually took, package by package. Without this a build-only
# tool that slips out of the purge list just quietly ships.
log "gate: no build-only package survived"
for p in $BUILD_ONLY; do
    status=$(dpkg-query -W -f='${db:Status-Status}' "$p" 2>/dev/null || true)
    [ "$status" = "installed" ] && die "build-only package '$p' is still installed"
done
log "ZFS module intact, no compiler or build-only package present"

# --- documentation --------------------------------------------------------
# The dpkg path-exclude rules from hook 0100 cover everything installed after
# they were written, but debootstrap's own unpack predates them.
log "stripping documentation"
find /usr/share/doc -mindepth 1 -maxdepth 1 -type d 2>/dev/null | while read -r d; do
    find "$d" -type f ! -name copyright -delete
    find "$d" -type l -delete
done
find /usr/share/doc -mindepth 1 -type d -empty -delete 2>/dev/null || true
rm -rf /usr/share/man /usr/share/info /usr/share/groff /usr/share/lintian \
       /usr/share/linda /usr/share/help /usr/share/doc-base

# --- locales --------------------------------------------------------------
log "stripping locales to en_US.UTF-8 and C.UTF-8"
find /usr/share/locale -mindepth 1 -maxdepth 1 -type d ! -name 'en_US*' -exec rm -rf {} + 2>/dev/null || true
find /usr/share/i18n/locales -mindepth 1 -maxdepth 1 -type f \
     ! -name 'en_US' ! -name 'C' ! -name 'i18n*' ! -name 'iso14651_t1*' ! -name 'translit_*' \
     -delete 2>/dev/null || true

# --- apt and package state -----------------------------------------------
log "clearing apt caches"
apt-get clean
rm -rf /var/lib/apt/lists/*
rm -rf /var/cache/apt/archives/*.deb /var/cache/apt/*.bin
rm -rf /var/cache/debconf/*-old /var/lib/dpkg/*-old

# --- identity that must be per-machine -----------------------------------
# A machine-id baked into the image means every Nightshade box presents the
# same DHCP client identity and the same systemd journal namespace. Truncating
# (not deleting) the file is the documented way to make systemd generate a
# fresh one at first boot.
log "clearing machine-id"
: >/etc/machine-id
rm -f /var/lib/dbus/machine-id
# dbus is not in the manifest, so /var/lib/dbus usually does not exist. Only
# wire up the compatibility symlink when it does; creating the directory here
# would just leave a stray path that dbus's own postinst would have made anyway.
if [ -d /var/lib/dbus ]; then
    ln -sf /etc/machine-id /var/lib/dbus/machine-id
fi

# --- transient state ------------------------------------------------------
log "clearing logs and temporary state"
find /var/log -type f -delete 2>/dev/null || true
rm -rf /tmp/* /var/tmp/* /root/.bash_history /root/.cache

# -x stops at mount boundaries, which keeps the bind-mounted /proc, /sys and
# /dev out of the total (and out of the walk -- they are slow and meaningless
# here).
log "rootfs stripped ($(du -sx -h / 2>/dev/null | tail -1 | cut -f1) on disk)"
