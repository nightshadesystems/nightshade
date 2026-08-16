#!/bin/sh
# System-level defaults for the installed OS: locale, sshd posture, kernel pin.
. /run/nightshade-build/hooks/lib.sh

KVER="$(kernel_version)"

# --- locale ---------------------------------------------------------------
log "generating en_US.UTF-8"
# Only these two ever exist. Everything else is stripped by hook 0900 and kept
# out of future installs by the dpkg path-exclude rules from hook 0100.
cat >/etc/locale.gen <<'EOF'
en_US.UTF-8 UTF-8
EOF
locale-gen
cat >/etc/default/locale <<'EOF'
LANG=en_US.UTF-8
LC_ALL=en_US.UTF-8
EOF

# --- sshd -----------------------------------------------------------------
log "disabling sshd by default"
systemctl disable ssh.service 2>/dev/null || true
systemctl disable ssh.socket 2>/dev/null || true
# systemctl's offline behaviour in a chroot varies; make the intent explicit so
# the image cannot ship with sshd enabled because a symlink survived.
rm -f /etc/systemd/system/multi-user.target.wants/ssh.service \
      /etc/systemd/system/sockets.target.wants/ssh.socket
if [ -e /etc/systemd/system/multi-user.target.wants/ssh.service ]; then
    die "sshd is still enabled after disable"
fi

# The openssh-server postinst generated host keys inside the build chroot. Ship
# them and every Nightshade box on earth shares one set of host keys, which
# makes SSH authentication meaningless against a MITM. Delete them and
# regenerate per-machine on first boot instead.
log "removing build-time ssh host keys"
rm -f /etc/ssh/ssh_host_*
cat >/etc/systemd/system/nightshade-ssh-keygen.service <<'EOF'
[Unit]
Description=Generate per-machine SSH host keys
Documentation=man:ssh-keygen(1)
ConditionPathExistsGlob=|!/etc/ssh/ssh_host_*_key
Before=ssh.service
Wants=ssh.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/ssh-keygen -A

[Install]
WantedBy=multi-user.target
EOF
mkdir -p /etc/systemd/system/multi-user.target.wants
ln -sf ../nightshade-ssh-keygen.service \
    /etc/systemd/system/multi-user.target.wants/nightshade-ssh-keygen.service

# --- kernel pin -----------------------------------------------------------
# The image ships no compiler and no DKMS (hook 0900 purges them), so the ZFS
# module cannot be rebuilt on the appliance. An `apt upgrade` that pulls a new
# kernel would therefore produce a system whose root pool has no driver. Freeze
# the kernel; kernel updates are an image-replacement operation in Nightshade.
#
# To deliberately move the kernel, delete this file, reinstall zfs-dkms plus
# linux-headers-amd64 and build-essential, upgrade, and confirm `modinfo zfs`
# before rebooting.
log "pinning kernel packages (built against $KVER)"
cat >/etc/apt/preferences.d/10-nightshade-kernel-pin <<EOF
# Built against $KVER. See /usr/share/doc/nightshade/kernel-pin (README) before
# changing this.
Package: linux-image-amd64 linux-image-*-amd64 linux-headers-amd64 linux-headers-*
Pin: release *
Pin-Priority: -1
EOF

# --- misc appliance defaults ---------------------------------------------
log "applying appliance defaults"
# A firewall with no console and a stuck fsck prompt is a truck roll.
systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true

log "system defaults applied"
