#!/bin/sh
# Install the package manifests.
#
# Split into two apt invocations on purpose: the zfs-dkms postinst compiles the
# module against whatever headers and compiler are configured at that moment.
# If everything went in as one transaction, apt would be free to configure
# zfs-dkms before build-essential, and the DKMS build would fail (or worse,
# silently defer) in a way that only shows up at boot.
. /run/nightshade-build/hooks/lib.sh

log "stage 1: kernel, headers and toolchain"
apt_install linux-image-amd64 linux-headers-amd64 build-essential dkms

KVER="$(kernel_version)"
log "kernel installed: $KVER"
[ -d "/lib/modules/$KVER/build" ] || die "no headers at /lib/modules/$KVER/build"

log "stage 2: remaining target, live and build-only packages"
# shellcheck disable=SC2046
apt_install $(read_manifest "$STAGING/packages.list") \
            $(read_manifest "$STAGING/packages-live.list") \
            $(read_manifest "$STAGING/packages-build.list")

log "recording installed package manifest"
dpkg-query -W -f='${Package}\t${Version}\t${Installed-Size}\n' \
    | sort >"$STAGING/out/packages-installed.txt"

log "packages installed"
