#!/bin/sh
# Establish apt/dpkg policy BEFORE any package is installed.
#
# Everything here is written as persistent configuration rather than one-shot
# build flags, so the rules keep holding on the installed system when the
# operator installs packages later. That is what makes the "no recommends" and
# "no docs" acceptance criteria survive past first boot.
. /run/nightshade-build/hooks/lib.sh

log "writing apt sources for $SUITE"
# deb822 format: this is the only sources file, so a stray sources.list from
# debootstrap must not linger and re-add a components-less entry.
rm -f /etc/apt/sources.list
mkdir -p /etc/apt/sources.list.d
cat >/etc/apt/sources.list.d/debian.sources <<EOF
Types: deb
URIs: $MIRROR
Suites: $SUITE $SUITE-updates
Components: main contrib non-free-firmware
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg

Types: deb
URIs: $SECURITY_MIRROR
Suites: $SUITE-security
Components: main contrib non-free-firmware
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
EOF

log "disabling recommends and suggests"
cat >/etc/apt/apt.conf.d/99-nightshade <<'EOF'
// Nightshade installs exactly what is asked for and nothing else. The package
// manifest in build/mkimage/packages.list is only meaningful if apt is not
// allowed to widen it behind our backs.
APT::Install-Recommends "false";
APT::Install-Suggests "false";
APT::AutoRemove::RecommendsImportant "false";
APT::AutoRemove::SuggestsImportant "false";
// Appliance: never keep a package cache on the running system.
Dir::Cache::pkgcache "";
Dir::Cache::srcpkgcache "";
EOF

log "excluding documentation from dpkg unpacks"
# path-exclude applies to every future dpkg unpack too, so packages installed
# post-install stay doc-free without another cleanup pass.
#
# NOTE: /usr/share/doc/*/copyright is deliberately re-included. Stripping
# copyright files out of a redistributed image would violate the license terms
# of most of what is in it (GPL et al. require the license text to travel with
# the binaries). It costs a few MB and is not negotiable.
cat >/etc/dpkg/dpkg.cfg.d/01-nightshade-nodoc <<'EOF'
path-exclude=/usr/share/doc/*
path-include=/usr/share/doc/*/copyright
path-exclude=/usr/share/man/*
path-exclude=/usr/share/info/*
path-exclude=/usr/share/groff/*
path-exclude=/usr/share/lintian/*
path-exclude=/usr/share/linda/*
path-exclude=/usr/share/help/*

path-exclude=/usr/share/locale/*
path-include=/usr/share/locale/en_US*
path-include=/usr/share/locale/locale.alias
EOF

log "pinning apt to the build suite"
cat >/etc/apt/preferences.d/00-nightshade-suite <<EOF
Package: *
Pin: release n=$SUITE
Pin-Priority: 900
EOF

log "apt-get update"
apt-get update

log "apt policy established"
