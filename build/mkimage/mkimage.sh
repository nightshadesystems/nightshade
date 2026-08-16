#!/usr/bin/env bash
#
# mkimage.sh — build a Nightshade OS live/installer ISO from scratch.
#
# debootstrap -> chroot hooks -> squashfs -> xorriso. No live-build: every step
# is visible here, which is the point. UEFI only; there is no BIOS/isolinux
# path anywhere in this pipeline.
#
# Must run as root on a Debian-family host (chroot, bind mounts, mknod). CI runs
# it in a privileged debian:trixie container; locally a WSL2 root shell works.
#
#   ./mkimage.sh -o dist/nightshade-0.1.0.iso -v 0.1.0
#
set -euo pipefail

# ---------------------------------------------------------------------------
# configuration
# ---------------------------------------------------------------------------

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="$(cd "$HERE/.." && pwd)"

SUITE="trixie"
MIRROR="http://deb.debian.org/debian"
SECURITY_MIRROR="http://security.debian.org/debian-security"
ISO_LABEL="NIGHTSHADE"
ARCH="amd64"

OUTPUT=""
VERSION=""
WORKDIR="/var/tmp/nightshade-build"
CACHE_DIR=""
INSTALLER_BIN=""
KEEP_WORK=0
REUSE_ROOTFS=0

usage() {
    cat >&2 <<EOF
usage: mkimage.sh -o OUTPUT.iso -v VERSION [options]

required:
  -o PATH        output ISO path
  -v VERSION     version string (e.g. 0.1.0), baked into os-release

options:
  -w DIR         work directory (default: $WORKDIR)
                 must be a native Linux filesystem: a 9p/drvfs mount such as
                 WSL's /mnt/c cannot hold a rootfs (no device nodes, no owners)
  -b PATH        nightshade-install binary to embed (default: auto-detect from
                 target/release, or omit and the live session drops to a shell)
  -c DIR         apt archive cache directory, reused across builds (CI)
  -m URL         Debian mirror (default: $MIRROR)
  -s SUITE       Debian suite (default: $SUITE)
  -k             keep the work directory on exit
  -r             DEVELOPER SHORTCUT: reuse the rootfs left by a previous -k run
                 instead of rebuilding it. Skips debootstrap and every chroot
                 hook, refreshes only the installer binary, then re-runs
                 squashfs and ISO assembly. Turns a 10-minute cycle into a
                 2-minute one while iterating on the installer or the boot menu.
                 Never use it for a release or in CI: the image it produces
                 reflects the package state of whenever the rootfs was made.
  -h             this help

environment:
  SOURCE_DATE_EPOCH   if set, timestamps are clamped to it for reproducibility
EOF
    exit 2
}

while getopts ":o:v:w:b:c:m:s:krh" opt; do
    case "$opt" in
        o) OUTPUT="$OPTARG" ;;
        v) VERSION="$OPTARG" ;;
        w) WORKDIR="$OPTARG" ;;
        b) INSTALLER_BIN="$OPTARG" ;;
        c) CACHE_DIR="$OPTARG" ;;
        m) MIRROR="$OPTARG" ;;
        s) SUITE="$OPTARG" ;;
        k) KEEP_WORK=1 ;;
        r) REUSE_ROOTFS=1; KEEP_WORK=1 ;;
        h) usage ;;
        :) echo "mkimage.sh: -$OPTARG requires an argument" >&2; usage ;;
        \?) echo "mkimage.sh: unknown option -$OPTARG" >&2; usage ;;
    esac
done

[ -n "$OUTPUT" ] || { echo "mkimage.sh: -o is required" >&2; usage; }
[ -n "$VERSION" ] || { echo "mkimage.sh: -v is required" >&2; usage; }

ROOTFS="$WORKDIR/rootfs"
ISOTREE="$WORKDIR/iso"
OUTDIR="$WORKDIR/out"
STAGING="$ROOTFS/run/nightshade-build"

: "${SOURCE_DATE_EPOCH:=$(date +%s)}"
export SOURCE_DATE_EPOCH
BUILD_ID="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M%S)"

# ---------------------------------------------------------------------------
# output helpers
# ---------------------------------------------------------------------------

STEP=0
step() {
    STEP=$((STEP + 1))
    printf '\n\033[1;35m==>\033[0m \033[1m[%d] %s\033[0m\n' "$STEP" "$*" >&2
}
info() { printf '    %s\n' "$*" >&2; }
die()  { printf '\n\033[1;31m!!\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# cleanup
# ---------------------------------------------------------------------------

# Bind mounts inside the rootfs must come down before anything touches the
# directory, and they must come down even when a hook fails. Leaking a bind of
# /dev into a directory that a later `rm -rf` visits is how build scripts eat
# the host.
MOUNTED=()

do_mount() {
    # The target is always the LAST argument, whichever option form is used:
    # "-t proc proc DIR", "--bind SRC DIR". Recording $2 would record the
    # source, and then unmount_all would have nothing it could match.
    local target="${*: -1}"
    mount "$@"
    MOUNTED+=("$target")
}

unmount_all() {
    local i m

    for (( i=${#MOUNTED[@]}-1; i>=0; i-- )); do
        m="${MOUNTED[i]}"
        if mountpoint -q "$m" 2>/dev/null; then
            umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
        fi
    done
    MOUNTED=()

    # Then sweep /proc/mounts for anything still mounted under the rootfs.
    # This is the part that actually keeps us safe, because it does not trust
    # the bookkeeping above. Longest paths first, so children come down before
    # their parents.
    while read -r m; do
        [ -n "$m" ] || continue
        umount "$m" 2>/dev/null || umount -l "$m" 2>/dev/null || true
    done < <(awk -v r="$ROOTFS/" '$2 ~ "^"r {print length($2), $2}' /proc/mounts \
             | sort -rn | cut -d' ' -f2-)
}

# Never delete a tree that still has something mounted inside it. A leaked bind
# of /dev plus `rm -rf` is how a build script destroys its host.
assert_nothing_mounted_under() {
    local root="$1" found
    found=$(awk -v r="$root/" '$2 ~ "^"r {print "  " $2}' /proc/mounts)
    if [ -n "$found" ]; then
        die "refusing to delete $root: these are still mounted underneath:
$found
  Unmount them and run again."
    fi
}

cleanup() {
    local rc=$?
    unmount_all
    if [ "$rc" -ne 0 ]; then
        printf '\n\033[1;31m!!\033[0m build failed (exit %d); work tree left at %s\n' \
            "$rc" "$WORKDIR" >&2
    elif [ "$KEEP_WORK" -eq 0 ]; then
        info "removing work tree $WORKDIR"
        assert_nothing_mounted_under "$WORKDIR"
        rm -rf "$WORKDIR"
    fi
    return $rc
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---------------------------------------------------------------------------
# preflight
# ---------------------------------------------------------------------------

step "preflight"

[ "$(id -u)" -eq 0 ] || die "must run as root (debootstrap, chroot and bind mounts need it)"

MISSING=()
for tool in debootstrap xorriso mksquashfs chroot; do
    command -v "$tool" >/dev/null 2>&1 || MISSING+=("$tool")
done
if [ "${#MISSING[@]}" -gt 0 ]; then
    die "missing host tools: ${MISSING[*]}
  Debian/Ubuntu: apt-get install -y debootstrap xorriso squashfs-tools"
fi

# debootstrap ships one script per suite; on an older host trixie may be absent
# even though the mirror has it.
if [ ! -e "/usr/share/debootstrap/scripts/$SUITE" ]; then
    die "debootstrap has no script for '$SUITE' (looked in /usr/share/debootstrap/scripts).
  On an older host: ln -s sid /usr/share/debootstrap/scripts/$SUITE"
fi

# A rootfs on 9p/drvfs/vfat silently loses ownership, permissions and device
# nodes, and the resulting image is unbootable in ways that are miserable to
# diagnose. Refuse up front.
mkdir -p "$WORKDIR"
WORK_FSTYPE="$(stat -f -c %T "$WORKDIR")"
case "$WORK_FSTYPE" in
    ext2/ext3|ext4|xfs|btrfs|tmpfs|zfs|overlayfs) ;;
    *) die "work directory $WORKDIR is on '$WORK_FSTYPE', which cannot hold a rootfs.
  Pick a native Linux filesystem with -w (e.g. -w /var/tmp/nightshade-build)." ;;
esac

AVAIL_MB=$(df -Pm "$WORKDIR" | awk 'NR==2 {print $4}')
[ "$AVAIL_MB" -ge 12000 ] || die "need ~12G free in $WORKDIR, have ${AVAIL_MB}M"

# Auto-detect the installer binary if one was not named explicitly.
if [ -z "$INSTALLER_BIN" ]; then
    for candidate in \
        "$BUILD_DIR/../target/release/nightshade-install" \
        "$BUILD_DIR/../target/x86_64-unknown-linux-gnu/release/nightshade-install"; do
        if [ -x "$candidate" ]; then
            INSTALLER_BIN="$candidate"
            break
        fi
    done
fi
if [ -n "$INSTALLER_BIN" ]; then
    [ -x "$INSTALLER_BIN" ] || die "installer binary not executable: $INSTALLER_BIN"
    info "installer: $INSTALLER_BIN"
else
    info "installer: none found (live session will drop to a shell)"
fi

info "suite=$SUITE arch=$ARCH version=$VERSION build=$BUILD_ID"
info "work=$WORKDIR ($WORK_FSTYPE, ${AVAIL_MB}M free)"
info "output=$OUTPUT"

# ---------------------------------------------------------------------------
# 1-5. rootfs construction
#
# Grouped into a function so that -r can skip the whole thing and go straight to
# image assembly against a rootfs left behind by an earlier run. Bash functions
# share the enclosing scope, so KVER and the MOUNTED array set in here are the
# same variables the rest of the script uses.
# ---------------------------------------------------------------------------

build_rootfs() {

step "debootstrap $SUITE into $ROOTFS"

# Idempotence: a half-finished rootfs from a previous failed run would poison
# everything downstream, so start clean rather than trying to resume.
if [ -d "$ROOTFS" ]; then
    info "removing previous rootfs"
    unmount_all
    assert_nothing_mounted_under "$ROOTFS"
    rm -rf "$ROOTFS"
fi
mkdir -p "$ROOTFS" "$ISOTREE" "$OUTDIR"

debootstrap \
    --arch="$ARCH" \
    --variant=minbase \
    --components=main,contrib,non-free-firmware \
    "$SUITE" "$ROOTFS" "$MIRROR"

# ---------------------------------------------------------------------------
# 2. prepare the chroot
# ---------------------------------------------------------------------------

step "preparing chroot"

do_mount -t proc  proc  "$ROOTFS/proc"
do_mount -t sysfs sys   "$ROOTFS/sys"
do_mount --bind /dev    "$ROOTFS/dev"
do_mount -t devpts devpts "$ROOTFS/dev/pts"

# Maintainer scripts must not start daemons against the build host's init.
cat >"$ROOTFS/usr/sbin/policy-rc.d" <<'EOF'
#!/bin/sh
exit 101
EOF
chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"
chroot "$ROOTFS" dpkg-divert --local --rename --add /usr/bin/ischroot >/dev/null
ln -sf /bin/true "$ROOTFS/usr/bin/ischroot"

cp /etc/resolv.conf "$ROOTFS/etc/resolv.conf"

# ---------------------------------------------------------------------------
# 3. stage the build payload
# ---------------------------------------------------------------------------

step "staging build payload"

mkdir -p "$STAGING/out" "$STAGING/bin" "$STAGING/stash"
cp -a "$HERE/hooks"            "$STAGING/hooks"
cp -a "$BUILD_DIR/branding"    "$STAGING/branding"
cp -a "$BUILD_DIR/grub"        "$STAGING/grub"
cp    "$HERE/packages.list"       "$STAGING/packages.list"
cp    "$HERE/packages-live.list"  "$STAGING/packages-live.list"
cp    "$HERE/packages-build.list" "$STAGING/packages-build.list"
[ -n "$INSTALLER_BIN" ] && install -m 0755 "$INSTALLER_BIN" "$STAGING/bin/nightshade-install"

cat >"$STAGING/build.env" <<EOF
VERSION='$VERSION'
BUILD_ID='$BUILD_ID'
SUITE='$SUITE'
MIRROR='$MIRROR'
SECURITY_MIRROR='$SECURITY_MIRROR'
ISO_LABEL='$ISO_LABEL'
ARCH='$ARCH'
SOURCE_DATE_EPOCH='$SOURCE_DATE_EPOCH'
EOF

if [ -n "$CACHE_DIR" ] && [ -d "$CACHE_DIR" ]; then
    count=$(find "$CACHE_DIR" -name '*.deb' | wc -l)
    if [ "$count" -gt 0 ]; then
        info "seeding apt cache with $count .deb files"
        cp -a "$CACHE_DIR"/*.deb "$ROOTFS/var/cache/apt/archives/" 2>/dev/null || true
    fi
fi

# ---------------------------------------------------------------------------
# 4. run the chroot hooks
# ---------------------------------------------------------------------------

step "running chroot hooks"

shopt -s nullglob
HOOKS=("$HERE/hooks"/0*.sh)
shopt -u nullglob
[ "${#HOOKS[@]}" -gt 0 ] || die "no hooks found in $HERE/hooks"

for hook in "${HOOKS[@]}"; do
    name="$(basename "$hook")"

    # The strip hook runs `apt-get clean`, so harvest the downloaded .debs for
    # the CI cache while they still exist.
    if [ -n "$CACHE_DIR" ] && [[ "$name" == *strip* ]]; then
        info "harvesting apt cache to $CACHE_DIR"
        mkdir -p "$CACHE_DIR"
        cp -an "$ROOTFS/var/cache/apt/archives"/*.deb "$CACHE_DIR/" 2>/dev/null || true
    fi

    info "hook: $name"
    chmod 0755 "$STAGING/hooks/$name"
    chroot "$ROOTFS" "/run/nightshade-build/hooks/$name" \
        || die "hook $name failed"
done

# ---------------------------------------------------------------------------
# 5. collect artifacts and tear the chroot down
# ---------------------------------------------------------------------------

step "collecting artifacts"

cp -a "$STAGING/out/." "$OUTDIR/"
[ -f "$OUTDIR/live/vmlinuz" ]   || die "hooks did not export a kernel"
[ -f "$OUTDIR/live/initrd.img" ] || die "hooks did not export an initramfs"
[ -f "$OUTDIR/esp.img" ]        || die "hooks did not export an ESP image"
KVER="$(cat "$OUTDIR/kver")"
info "kernel $KVER"
info "$(wc -l <"$OUTDIR/packages-installed.txt") packages in the image"

step "tearing down chroot"

rm -f "$ROOTFS/usr/sbin/policy-rc.d"
rm -f "$ROOTFS/usr/bin/ischroot"
chroot "$ROOTFS" dpkg-divert --local --rename --remove /usr/bin/ischroot >/dev/null
# resolv.conf is host-specific; the installed system gets its own.
: >"$ROOTFS/etc/resolv.conf"
rm -rf "$STAGING"
unmount_all

}   # end build_rootfs

if [ "$REUSE_ROOTFS" -eq 1 ]; then
    step "reusing existing rootfs (developer shortcut)"
    [ -x "$ROOTFS/bin/sh" ] \
        || die "-r given but there is no usable rootfs at $ROOTFS; run once without -r"
    for required in live/vmlinuz live/initrd.img esp.img kver packages-installed.txt; do
        [ -e "$OUTDIR/$required" ] \
            || die "-r given but $OUTDIR/$required is missing; run once without -r"
    done
    info "rootfs built $(stat -c %y "$ROOTFS" | cut -d. -f1)"

    # The installer binary is the one thing that actually changes between
    # iterations, so refresh it in place rather than re-running the hooks.
    if [ -n "$INSTALLER_BIN" ]; then
        info "refreshing /usr/local/bin/nightshade-install"
        install -m 0755 "$INSTALLER_BIN" "$ROOTFS/usr/local/bin/nightshade-install"
    fi
    KVER="$(cat "$OUTDIR/kver")"
else
    build_rootfs
fi

# ---------------------------------------------------------------------------
# 6. squashfs
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# 5b. verify the installer actually runs in the image
# ---------------------------------------------------------------------------

# The installer is compiled on the build host, but it has to run inside the
# image. If the host's glibc is newer than the image's -- which it is whenever
# the build runs anywhere other than a debian:trixie container -- the binary
# links fine and then dies at runtime with a version-symbol error, on a console
# with nothing else on it. Prove it executes now instead.
if [ -x "$ROOTFS/usr/local/bin/nightshade-install" ]; then
    step "verifying the installer runs inside the image"
    if ! chroot "$ROOTFS" /usr/local/bin/nightshade-install --version; then
        die "the installer binary does not run inside the image.
  It was almost certainly built against a newer glibc than Debian $SUITE ships.
  Build it in a debian:$SUITE container (as CI does), or statically."
    fi
fi

step "compressing rootfs to squashfs"

# A live bind mount here would be walked and baked into the image: the squashfs
# would carry a snapshot of the build host's /proc and /dev, and the ISO would
# be both enormous and wrong.
assert_nothing_mounted_under "$ROOTFS"

mkdir -p "$ISOTREE/live"
SQUASH_ARGS=(
    -comp zstd -Xcompression-level 19
    -b 1M
    -noappend
    -no-progress
)
# Timestamps come from SOURCE_DATE_EPOCH, which mksquashfs reads from the
# environment on its own (>= 4.5). Passing -mkfs-time/-all-time as well is a
# hard error: "SOURCE_DATE_EPOCH and command line options can't be used at the
# same time to set timestamp(s)".
#
# -e consumes every remaining argument as an exclude path, so it has to be the
# last option on the line. Anything appended after it would be silently treated
# as a file to exclude instead of as a flag.
SQUASH_ARGS+=(-e "var/cache/apt/archives")

mksquashfs "$ROOTFS" "$ISOTREE/live/filesystem.squashfs" "${SQUASH_ARGS[@]}"

SQUASH_MB=$(( $(stat -c %s "$ISOTREE/live/filesystem.squashfs") / 1048576 ))
info "squashfs is ${SQUASH_MB}M"

# live-boot reads this to size its overlay; harmless if absent, cheap to write.
du -sx --block-size=1 "$ROOTFS" | cut -f1 >"$ISOTREE/live/filesystem.size"

# ---------------------------------------------------------------------------
# 7. assemble the ISO tree
# ---------------------------------------------------------------------------

step "assembling ISO tree"

cp "$OUTDIR/live/vmlinuz"    "$ISOTREE/live/vmlinuz"
cp "$OUTDIR/live/initrd.img" "$ISOTREE/live/initrd.img"

mkdir -p "$ISOTREE/boot/grub/fonts" "$ISOTREE/boot/grub/themes" "$ISOTREE/EFI/BOOT"
cp "$OUTDIR"/grub/fonts/*.pf2 "$ISOTREE/boot/grub/fonts/"
# From the hook's output, not straight from the source tree: the hook scales the
# logo to its display size and drops the oversized original.
cp -a "$OUTDIR/grub/theme" "$ISOTREE/boot/grub/themes/nightshade"
cp "$OUTDIR/grub/efi/EFI/BOOT/BOOTX64.EFI" "$ISOTREE/EFI/BOOT/BOOTX64.EFI"

sed -e "s|@VERSION@|$VERSION|g" \
    -e "s|@LABEL@|$ISO_LABEL|g" \
    "$BUILD_DIR/grub/grub.cfg.in" >"$ISOTREE/boot/grub/grub.cfg"

mkdir -p "$ISOTREE/.disk"
printf 'Nightshade OS %s (%s) %s\n' "$VERSION" "$BUILD_ID" "$ARCH" >"$ISOTREE/.disk/info"
cp "$OUTDIR/packages-installed.txt" "$ISOTREE/.disk/packages.txt"

# ---------------------------------------------------------------------------
# 8. xorriso
# ---------------------------------------------------------------------------

step "building ISO"

mkdir -p "$(dirname "$OUTPUT")"
rm -f "$OUTPUT"

# UEFI only. The ESP is appended as a real GPT partition and El Torito points
# at that same appended interval, so one FAT image serves both the optical boot
# path and a plain `dd` of the ISO onto a USB stick.
XORRISO_ARGS=(
    -as mkisofs
    -iso-level 3
    -full-iso9660-filenames
    -rational-rock
    -joliet -joliet-long
    -volid "$ISO_LABEL"
    -appended_part_as_gpt
    -append_partition 2 C12A7328-F81F-11D2-BA4B-00A0C93EC93B "$OUTDIR/esp.img"
    -e '--interval:appended_partition_2:all::'
    -no-emul-boot
    -partition_offset 16
)
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    XORRISO_ARGS+=(--modification-date="$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y%m%d%H%M%S00)")
fi

xorriso "${XORRISO_ARGS[@]}" -output "$OUTPUT" "$ISOTREE"

[ -f "$OUTPUT" ] || die "xorriso did not produce $OUTPUT"

# ---------------------------------------------------------------------------
# 9. verify
# ---------------------------------------------------------------------------

step "verifying ISO"

# Prove the firmware-visible pieces are actually there. A UEFI ISO that is
# missing its ESP partition entry boots to a black screen and nothing else.
if ! xorriso -indev "$OUTPUT" -report_system_area plain 2>/dev/null | grep -qi 'GPT'; then
    die "ISO has no GPT partition table; UEFI firmware will not find the ESP"
fi
xorriso -indev "$OUTPUT" -find /EFI/BOOT/BOOTX64.EFI >/dev/null 2>&1 \
    || die "ISO is missing /EFI/BOOT/BOOTX64.EFI"

ISO_MB=$(( $(stat -c %s "$OUTPUT") / 1048576 ))
sha256sum "$OUTPUT" >"$OUTPUT.sha256"

printf '\n\033[1;32m==>\033[0m \033[1mNightshade OS %s\033[0m\n' "$VERSION" >&2
info "iso     $OUTPUT (${ISO_MB}M)"
info "sha256  $(cut -d' ' -f1 <"$OUTPUT.sha256")"
info "kernel  $KVER"
