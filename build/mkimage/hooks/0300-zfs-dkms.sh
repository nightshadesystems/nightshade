#!/bin/sh
# Build the ZFS kernel module at image build time and gate the build on it.
#
# The whole point is that neither the live environment nor the installed system
# ever compiles anything: a firewall appliance that has to run DKMS on first
# boot is one kernel bump away from an unbootable ZFS root, with no console to
# debug it from. So we compile here, prove it worked, and stash the artifacts
# where hook 0700 can put them back after it purges the toolchain.
. /run/nightshade-build/hooks/lib.sh

KVER="$(kernel_version)"
log "building ZFS DKMS module for $KVER"

# The zfs-dkms postinst normally autoinstalls, but it tolerates failure in some
# configurations. Run it explicitly so a build failure surfaces here with its
# real output rather than as a missing module three hooks later.
if ! dkms status -k "$KVER" | grep -q '^zfs.*installed'; then
    log "dkms autoinstall did not leave zfs installed; building explicitly"
    ZFS_VER="$(dkms status | sed -n 's|^zfs[/,] *\([^,:]*\).*|\1|p' | head -n1)"
    [ -n "$ZFS_VER" ] || die "zfs-dkms is not registered with dkms at all"
    log "zfs-dkms version $ZFS_VER"
    dkms build -m zfs -v "$ZFS_VER" -k "$KVER"
    dkms install -m zfs -v "$ZFS_VER" -k "$KVER"
fi

log "dkms status: $(dkms status -k "$KVER" | tr '\n' ' ')"

depmod -a "$KVER"

# --- build gate -----------------------------------------------------------
# modinfo resolves through modules.dep, so this proves the module exists, is
# built for the right kernel, AND is wired into the dependency index. A bare
# file existence check would pass on all three being wrong.
log "gate: modinfo zfs"
if ! modinfo -k "$KVER" zfs >"$STAGING/out/modinfo-zfs.txt" 2>&1; then
    cat "$STAGING/out/modinfo-zfs.txt" >&2
    die "modinfo zfs failed: the ZFS module did not build for $KVER"
fi
sed -n 's/^version:[[:space:]]*/zfs module version /p' "$STAGING/out/modinfo-zfs.txt" | head -n1 | while read -r l; do log "$l"; done

# --- stash for hook 0900 --------------------------------------------------
# Purging zfs-dkms runs `dkms remove`, which deletes these. Copy them out now
# and 0900 restores them after the purge.
DKMS_DIR="/lib/modules/$KVER/updates/dkms"
[ -d "$DKMS_DIR" ] || die "expected DKMS output at $DKMS_DIR"
log "stashing $(find "$DKMS_DIR" -type f | wc -l) built modules"
mkdir -p "$STAGING/stash/modules"
cp -a "$DKMS_DIR" "$STAGING/stash/modules/dkms"
echo "$KVER" >"$STAGING/stash/kver"

# --- package the built modules -------------------------------------------
# zfs-initramfs depends on "zfs-modules | zfs-dkms". zfs-modules is a virtual
# package that exists precisely so prebuilt module packages can satisfy it, and
# zfs-dkms is only one of the things that can Provide it.
#
# Without this, purging zfs-dkms in hook 0900 drags zfs-initramfs out with it.
# That is not a cosmetic loss: zfs-initramfs is what puts the zpool binary and
# the pool-import script into the initramfs, and an image without it boots to
# "ALERT! ZFS=rpool/ROOT/nightshade does not exist. Dropping to a shell!".
#
# So ship the modules we just built as a real package that Provides:
# zfs-modules. dpkg owns the files, the dependency is honestly satisfied, and
# the toolchain can still go.
ZFS_VERSION="$(modinfo -k "$KVER" -F version zfs | head -n1)"
PKG="$STAGING/zfs-modules-pkg"
log "packaging modules as nightshade-zfs-modules ($ZFS_VERSION)"

rm -rf "$PKG"
mkdir -p "$PKG/DEBIAN" "$PKG/lib/modules/$KVER/updates/dkms"
cp -a "$DKMS_DIR/." "$PKG/lib/modules/$KVER/updates/dkms/"

cat >"$PKG/DEBIAN/control" <<EOF
Package: nightshade-zfs-modules
Version: ${ZFS_VERSION}-nightshade1
Architecture: $ARCH
Section: kernel
Priority: optional
Maintainer: Nightshade Systems <noreply@quartz.systems>
Provides: zfs-modules
Depends: linux-image-$KVER
Description: Prebuilt OpenZFS kernel modules for Nightshade OS
 OpenZFS modules compiled against $KVER during the Nightshade image build.
 .
 Nightshade ships no compiler and no DKMS: the modules are built once, in the
 build chroot, and delivered here. This package Provides zfs-modules so that
 zfs-initramfs is satisfied without zfs-dkms.
 .
 Because there is no DKMS, these modules only match the kernel they were built
 for, which is why linux-image-amd64 is pinned in
 /etc/apt/preferences.d/10-nightshade-kernel-pin.
EOF

cat >"$PKG/DEBIAN/postinst" <<EOF
#!/bin/sh
set -e
depmod -a "$KVER"
EOF
chmod 0755 "$PKG/DEBIAN/postinst"

dpkg-deb --build --root-owner-group "$PKG" "$STAGING/nightshade-zfs-modules.deb" >/dev/null
[ -s "$STAGING/nightshade-zfs-modules.deb" ] || die "failed to build the modules package"

log "ZFS module built, verified and packaged"
