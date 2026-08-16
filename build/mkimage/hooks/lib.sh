# Shared prelude for Nightshade chroot hooks.
#
# Sourced (not executed) by every 0*.sh hook. Hooks run INSIDE the target
# chroot with the build staging tree mounted at /run/nightshade-build, so all
# paths here are chroot-relative.

set -euo pipefail

STAGING=/run/nightshade-build

# build.env is written by mkimage.sh and carries VERSION, BUILD_ID, SUITE,
# MIRROR, SOURCE_DATE_EPOCH and friends.
# shellcheck source=/dev/null
. "$STAGING/build.env"

export DEBIAN_FRONTEND=noninteractive
export LC_ALL=C.UTF-8
export LANG=C.UTF-8

_hook_name="$(basename "${0:-hook}")"

log() {
    printf '  [%s] %s\n' "$_hook_name" "$*" >&2
}

die() {
    printf '  [%s] FATAL: %s\n' "$_hook_name" "$*" >&2
    exit 1
}

# Read a package manifest, dropping comments and blank lines.
read_manifest() {
    local file="$1"
    [ -r "$file" ] || die "manifest not found: $file"
    sed -e 's/#.*//' -e 's/[[:space:]]\+$//' -e '/^$/d' "$file"
}

# Resolve the exact kernel version this image was built against. There is
# exactly one kernel in the image; more than one means the manifest drifted and
# we would silently pick the wrong one for DKMS, so treat it as fatal.
kernel_version() {
    local count
    count=$(find /lib/modules -mindepth 1 -maxdepth 1 -type d | wc -l)
    [ "$count" -eq 1 ] || die "expected exactly 1 kernel in /lib/modules, found $count"
    basename "$(find /lib/modules -mindepth 1 -maxdepth 1 -type d)"
}

apt_install() {
    apt-get install --yes --no-install-recommends "$@"
}
