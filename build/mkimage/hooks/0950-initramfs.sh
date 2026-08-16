#!/bin/sh
# Build the initramfs and export the boot artifacts for ISO assembly.
#
# Runs LAST, after hook 0900 has purged the build toolchain and restored the
# ZFS module. Building it earlier would bake in whatever the tree looked like
# mid-purge: apt's own initramfs triggers fire while zfs-dkms is being removed,
# at a moment when the module files have been deleted and not yet put back.
#
# This initramfs serves both roles: live-boot's hooks let it find and mount
# /live/filesystem.squashfs off the ISO, and zfs-initramfs's hooks let the very
# same image import a root pool after installation. One initramfs, both paths,
# which is why the installer can reuse the live rootfs verbatim.
. /run/nightshade-build/hooks/lib.sh

KVER="$(kernel_version)"

log "configuring initramfs-tools"
cat >/etc/initramfs-tools/conf.d/nightshade.conf <<'EOF'
COMPRESS=zstd
MODULES=most
EOF

# Prove the hooks we depend on are present before spending a minute building an
# initramfs that would silently fail to boot.
for hook in /usr/share/initramfs-tools/hooks/live \
            /usr/share/initramfs-tools/hooks/zfs \
            /usr/share/initramfs-tools/scripts/local-top/zfs; do
    [ -f "$hook" ] || die "missing initramfs hook: $hook"
done
log "live-boot and zfs initramfs hooks present"

log "building initramfs for $KVER"
update-initramfs -c -k "$KVER" 2>&1 | sed 's/^/    /' >&2

INITRD="/boot/initrd.img-$KVER"
KERNEL="/boot/vmlinuz-$KVER"
[ -f "$INITRD" ] || die "initramfs not produced at $INITRD"
[ -f "$KERNEL" ] || die "kernel not found at $KERNEL"

# --- gate -----------------------------------------------------------------
# Check for the actual payload, not just the string "zfs" somewhere in a path.
# A loose match passes on an initramfs that happens to contain an unrelated
# filename and tells you nothing about whether the pool can be imported.
log "gate: verifying initramfs contents"
if ! lsinitramfs "$INITRD" >"$STAGING/out/initramfs-contents.txt" 2>&1; then
    die "lsinitramfs failed on $INITRD"
fi

check_initramfs() {
    grep -q "$1" "$STAGING/out/initramfs-contents.txt" \
        || die "initramfs is missing $1 ($2)"
}

check_initramfs 'scripts/live'          'live-boot: the ISO would not boot'
check_initramfs 'sbin/zpool'            'zfs-initramfs: no pool could be imported'
check_initramfs 'sbin/zfs'              'zfs-initramfs: no dataset could be mounted'
check_initramfs 'scripts/local-top/zfs' 'zfs-initramfs: the import script is absent'
check_initramfs 'zfs\.ko'               'the ZFS kernel module itself'
log "initramfs carries live-boot and a complete ZFS stack"

log "exporting kernel and initramfs"
mkdir -p "$STAGING/out/live"
cp "$KERNEL" "$STAGING/out/live/vmlinuz"
cp "$INITRD" "$STAGING/out/live/initrd.img"
echo "$KVER" >"$STAGING/out/kver"

# No `file` here: it belongs to the build toolchain that hook 0900 purged.
log "initramfs built ($(du -h "$INITRD" | cut -f1))"
