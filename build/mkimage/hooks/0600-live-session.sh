#!/bin/sh
# Live-environment session: auto-login root on tty1 and launch the installer.
#
# Everything this hook creates is live-only and is recorded in
# /usr/share/nightshade/live-manifest. The installer reads that manifest when it
# strips the live layer out of the target rootfs, so there is exactly one place
# that knows what "live-only" means.
. /run/nightshade-build/hooks/lib.sh

mkdir -p /usr/share/nightshade /usr/local/bin

# --- installer binary -----------------------------------------------------
if [ -x "$STAGING/bin/nightshade-install" ]; then
    log "installing nightshade-install binary"
    install -m 0755 "$STAGING/bin/nightshade-install" /usr/local/bin/nightshade-install
else
    # Milestone 1 builds the ISO before the Rust crate exists. The live session
    # must still come up so the pipeline itself can be tested; it drops to a
    # shell and says why.
    log "WARNING: no installer binary staged; live session will drop to a shell"
fi

# --- session wrapper ------------------------------------------------------
log "installing live session wrapper"
cat >/usr/local/bin/nightshade-live-session <<'EOF'
#!/bin/sh
# Runs the installer on the live console, then always leaves the operator with
# a usable shell instead of a dead tty.

INSTALLER=/usr/local/bin/nightshade-install

printf '\n'
if [ -x "$INSTALLER" ]; then
    "$INSTALLER"
    rc=$?
    printf '\n'
    if [ "$rc" -eq 0 ]; then
        printf '  Installer exited normally.\n'
    else
        printf '  Installer exited with status %s.\n' "$rc"
        printf '  A full log was written to /tmp/nightshade-install.log\n'
    fi
else
    printf '  This image was built without an installer binary.\n'
    printf '  Build it with `make installer` and rebuild the ISO.\n'
fi

printf '\n'
printf '  Dropping to a root shell on the live system.\n'
printf '  Relaunch the installer at any time with:  nightshade-install\n'
printf '\n'

# The installer turns terminal echo off while a password is typed and restores
# it on the way out. A Ctrl-C at exactly that moment kills it before the restore
# runs, and the operator inherits a shell that does not echo what they type.
stty sane 2>/dev/null || true

exec /bin/bash -l
EOF
chmod 0755 /usr/local/bin/nightshade-live-session

# --- systemd unit ---------------------------------------------------------
log "installing nightshade-installer.service"
cat >/etc/systemd/system/nightshade-installer.service <<'EOF'
[Unit]
Description=Nightshade OS installer (live session)
# Take tty1 away from the normal getty rather than racing it for the console.
Conflicts=getty@tty1.service
After=getty@tty1.service systemd-user-sessions.service
Before=getty.target

[Service]
# Type=idle keeps the banner from being shredded by boot log output.
Type=idle
ExecStart=/usr/local/bin/nightshade-live-session
StandardInput=tty-force
StandardOutput=tty
StandardError=tty
TTYPath=/dev/tty1
TTYReset=yes
TTYVHangup=yes
KillMode=process
IgnoreSIGPIPE=no
# The wrapper execs a shell and is meant to stay alive; restarting it would
# fight the operator for the tty.
Restart=no

[Install]
WantedBy=multi-user.target
EOF
mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf ../nightshade-installer.service \
    /etc/systemd/system/multi-user.target.wants/nightshade-installer.service

# --- serial console -------------------------------------------------------
# Root auto-login on ttyS0, live image only. This is what makes the ISO
# testable headlessly (make test-vm drives QEMU over serial) and what gives a
# remote hands operator a console on a box with no video.
log "enabling root auto-login on ttyS0 (live only)"
mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d
cat >/etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf <<'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear --keep-baud 115200,38400,9600 %I $TERM
EOF

# --- live manifest --------------------------------------------------------
log "recording live-only manifest"
cat >/usr/share/nightshade/live-manifest <<'EOF'
# Live-environment-only artifacts, removed from the target during installation.
# Lines are either "pkg <name>" (apt-get purge) or "path <absolute path>" (rm -rf).
pkg live-boot
pkg live-boot-initramfs-tools
pkg live-config
pkg live-config-systemd
path /etc/systemd/system/nightshade-installer.service
path /etc/systemd/system/multi-user.target.wants/nightshade-installer.service
path /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
path /usr/local/bin/nightshade-live-session
path /usr/local/bin/nightshade-install
path /usr/share/nightshade/live-manifest
EOF

log "live session configured"
