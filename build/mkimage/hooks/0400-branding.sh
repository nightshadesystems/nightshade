#!/bin/sh
# Apply Nightshade identity: os-release, console banners, hostname.
. /run/nightshade-build/hooks/lib.sh

log "installing os-release (version $VERSION, build $BUILD_ID)"
# /usr/lib/os-release is the canonical location; /etc/os-release is the
# compatibility symlink that everything actually reads.
sed -e "s|@VERSION@|$VERSION|g" \
    -e "s|@BUILD_ID@|$BUILD_ID|g" \
    "$STAGING/branding/os-release" >/usr/lib/os-release
ln -sf ../usr/lib/os-release /etc/os-release

# Debian's base-files ships these and they name the wrong distribution.
rm -f /etc/debian_version.orig
printf '%s\n' "$VERSION" >/etc/nightshade_version

log "installing console banners"
install -m 0644 "$STAGING/branding/issue" /etc/issue
install -m 0644 "$STAGING/branding/motd" /etc/motd
# issue.net is what sshd shows pre-auth; it must not carry agetty \-escapes
# because sshd prints the file verbatim.
sed -e 's/\\S{VERSION_ID}/'"$VERSION"'/g' -e 's/(\\s \\r \\m)//' \
    -e '/tty:/d' "$STAGING/branding/issue" >/etc/issue.net

# Debian assembles a dynamic motd from these; leaving them enabled would append
# Debian's own text under the Nightshade banner.
if [ -d /etc/update-motd.d ]; then
    find /etc/update-motd.d -type f -exec chmod -x {} +
fi
rm -f /var/run/motd.dynamic /run/motd.dynamic

log "setting hostname to nightshade"
printf 'nightshade\n' >/etc/hostname
cat >/etc/hosts <<'EOF'
127.0.0.1	localhost
127.0.1.1	nightshade

::1		localhost ip6-localhost ip6-loopback
ff02::1		ip6-allnodes
ff02::2		ip6-allrouters
EOF

log "branding applied"
