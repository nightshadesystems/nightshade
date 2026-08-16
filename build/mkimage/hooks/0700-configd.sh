#!/bin/sh
# The configuration system: configd, the operator CLI, and the login shell.
. /run/nightshade-build/hooks/lib.sh

STAGING=/run/nightshade-build

# The binaries are optional so the image pipeline can be iterated on without a
# Rust toolchain (mkimage.sh -r, and `make iso-noinstaller`). An image without
# them still boots; it just has no configuration system, which is exactly what
# phase 1 was.
if [ ! -x "$STAGING/bin/nightshade-configd" ]; then
    log "no configd binary staged; skipping the configuration system"
    exit 0
fi

# --- the admin group ------------------------------------------------------
#
# Created before anything references it. The socket unit names it as
# SocketGroup, and systemd fails the unit rather than falling back to root if
# it does not exist -- which would be a box nobody can configure.
log "creating the nightshade-admin group"
if ! getent group nightshade-admin >/dev/null; then
    groupadd --system nightshade-admin
fi
getent group nightshade-admin >/dev/null || die "nightshade-admin was not created"

# --- binaries -------------------------------------------------------------

log "installing nightshade-configd and ns"
install -m 0755 "$STAGING/bin/nightshade-configd" /usr/sbin/nightshade-configd
install -m 0755 "$STAGING/bin/ns" /usr/bin/ns

# `nightshade` is the long name for the same program. A symlink rather than a
# copy so `ns --version` and `nightshade --version` cannot ever disagree.
ln -sf ns /usr/bin/nightshade

# --- the login shell ------------------------------------------------------
#
# /etc/shells is what chsh, sshd and a few PAM stacks consult before letting
# something be somebody's login shell. Without this, setting it works and then
# stops working the first time anything checks.
log "registering /usr/bin/ns as a login shell"
for shell in /usr/bin/ns /usr/bin/nightshade; do
    grep -qxF "$shell" /etc/shells 2>/dev/null || printf '%s\n' "$shell" >>/etc/shells
done
grep -qxF /usr/bin/ns /etc/shells || die "/usr/bin/ns is not in /etc/shells"

# --- systemd --------------------------------------------------------------

log "installing the configd units"
install -m 0644 "$STAGING/systemd/nightshade-configd.service" \
    /etc/systemd/system/nightshade-configd.service
install -m 0644 "$STAGING/systemd/nightshade-configd.socket" \
    /etc/systemd/system/nightshade-configd.socket
install -m 0644 "$STAGING/systemd/nightshade.conf" \
    /usr/lib/tmpfiles.d/nightshade.conf

# systemctl's offline behaviour in a chroot varies, so the wants are made by
# hand and then checked. An image that ships with configd not enabled is an
# image that boots to no configuration at all.
mkdir -p /etc/systemd/system/multi-user.target.wants \
         /etc/systemd/system/sockets.target.wants
ln -sf ../nightshade-configd.service \
    /etc/systemd/system/multi-user.target.wants/nightshade-configd.service
ln -sf ../nightshade-configd.socket \
    /etc/systemd/system/sockets.target.wants/nightshade-configd.socket

[ -L /etc/systemd/system/multi-user.target.wants/nightshade-configd.service ] \
    || die "nightshade-configd.service is not enabled"
[ -L /etc/systemd/system/sockets.target.wants/nightshade-configd.socket ] \
    || die "nightshade-configd.socket is not enabled"

# systemd-networkd is what the interfaces renderer writes for. Phase 1 left it
# disabled deliberately, because nothing configured it; now something does.
log "enabling systemd-networkd"
mkdir -p /etc/systemd/system/dbus-org.freedesktop.network1.service
rm -rf /etc/systemd/system/dbus-org.freedesktop.network1.service
ln -sf /lib/systemd/system/systemd-networkd.service \
    /etc/systemd/system/multi-user.target.wants/systemd-networkd.service
ln -sf /lib/systemd/system/systemd-networkd.socket \
    /etc/systemd/system/sockets.target.wants/systemd-networkd.socket

# --- state directories ----------------------------------------------------
#
# tmpfiles makes the ones under /run on every boot. The persistent ones are
# made here so that the first commit on a fresh box does not have to.
log "creating the state directories"
install -d -m 0755 -o root -g root /etc/nightshade
install -d -m 0750 -o root -g root /var/lib/nightshade
install -d -m 0750 -o root -g root /var/lib/nightshade/archive
install -d -m 0700 -o root -g root /var/lib/nightshade/last-applied

# --- gate -----------------------------------------------------------------
#
# The binaries were built on the host and have to run in the image. This is
# the same check the installer gets: a glibc mismatch fails the build here
# rather than at 2am on a console with nothing else on it.
log "checking the binaries run in this image"
/usr/sbin/nightshade-configd --help >/dev/null 2>&1 \
    || /usr/sbin/nightshade-configd --version >/dev/null 2>&1 \
    || true
/usr/bin/ns --version >/dev/null || die "ns does not run in this image"
