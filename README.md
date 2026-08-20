# Nightshade

A minimal Debian 13 (trixie) based firewall operating system, built from
scratch: a reproducible live ISO pipeline, a branded GRUB, a Rust installer
that puts the system on disk with ZFS — optionally as a two-disk RAID1 mirror —
and a configuration system with a candidate/commit/rollback model behind a
modal operator CLI.

Phases 1 and 2 are here: **base image + installer**, and **the configuration
system**. There is no API frontend yet; the protocol is shaped so that adding
one is additive.

---

## Layout

```
build/
  mkimage/          debootstrap -> squashfs -> ISO pipeline
    mkimage.sh      the whole build, top to bottom
    packages.list   explicit manifest for the installed system
    packages-live.list    extra packages for the live/installer environment
    packages-build.list   build-only packages, purged before the image is sealed
    hooks/          numbered chroot customisation steps
  grub/
    theme/          Nightshade GRUB theme
    grub.cfg.in     ISO boot menu template
    default-grub.in /etc/default/grub for the installed system
  branding/         os-release, issue, motd
schema/             the configuration schema, in YAML. The only place a
                    config node is defined.
dist/systemd/       configd's units and tmpfiles rules
fuzz/               cargo-fuzz targets, in their own workspace
src/
  nightshade-common/      paths, shared constants
  nightshade-schema/      schema, validation, the curly-brace config format
  nightshade-proto/       the framed-CBOR protocol
  nightshade-render/      config -> systemd-networkd and system settings
  nightshade-ifstate/     the `show interfaces` model and its renderers
  nightshade-ifprobe/     reads interface state from netlink, ethtool, sysfs
  nightshade-configd/     the configuration daemon
  nightshade-cli/         `ns`, the operator CLI
  nightshade-installer/   the installer
```

---

## Requirements

The ISO build needs a **Debian-family host, running as root**, because it
drives `debootstrap`, `chroot` and bind mounts directly. It does *not* need
loop devices: the ESP image is built with `mtools` and the filesystem with
`mksquashfs`, both of which operate on plain files.

Host tools: `debootstrap`, `xorriso`, `mksquashfs`, plus `qemu-system-x86_64`
and OVMF for the test targets.

```sh
apt-get install -y debootstrap xorriso squashfs-tools qemu-system-x86 ovmf
```

On Windows, drive everything through WSL2 as root — it has `/dev/kvm`, so the
QEMU targets run accelerated:

```sh
wsl -d Ubuntu -u root make iso
```

Note that the repository often lives on `/mnt/c` there. That is fine for
sources, but the build work tree must be on a real Linux filesystem; a 9p mount
cannot hold device nodes or ownership. `mkimage.sh` checks this and refuses
rather than producing a subtly broken image. `WORKDIR` already defaults to
`/var/tmp/nightshade-build`.

---

## Building

```sh
make installer     # the installer alone
make configd       # nightshade-configd and ns
make iso           # full ISO -> dist/nightshade-0.1.0-202608160925.iso
make iso RELEASE=1 # release naming -> dist/nightshade-0.1.0.iso
make test          # cargo test --workspace
make test-vm       # boot the ISO in QEMU/OVMF with two blank 20G disks
make clean
```

`make help` lists everything, including the degraded-mirror test targets.

Useful variables: `VERSION`, `RELEASE`, `WORKDIR`, `VM_DIR`, `APT_CACHE`,
`CARGO_TARGET_DIR`, and `VM_DISPLAY=gtk` for a window instead of a serial
console.

### Versioning and ISO names

The **`VERSION`** file at the repository root is the single source of truth.

| build | name |
|---|---|
| release (`RELEASE=1`, or a CI tag build) | `nightshade-0.1.0.iso` |
| everything else | `nightshade-0.1.0-202608160925.iso` |

Development builds carry a `YYYYMMDDHHMM` UTC stamp, because otherwise every
build of 0.1.0 has the same filename and there is no telling which one is on
the USB stick — and a date alone is not enough when you cut several in an
afternoon. The stamp is derived from `SOURCE_DATE_EPOCH`, not from the wall
clock, so rebuilding a given commit reproduces the same name as well as the
same bytes; CI sets it to the commit date.

Tagging is checked: a `v0.2.0` tag whose `VERSION` still says `0.1.0` fails the
build rather than producing an ISO whose filename disagrees with its
`os-release`. Update `VERSION` (and the workspace `version` in `Cargo.toml`,
which CI warns about separately) before tagging.

Each build writes three files:

```
dist/nightshade-0.1.0-20260816.iso
dist/nightshade-0.1.0-20260816.iso.sha256
dist/nightshade-0.1.0-20260816.iso.packages.txt   # 264 packages, name + version
```

While iterating, `mkimage.sh -r` reuses the rootfs from a previous `-k` run and
re-runs only the installer refresh, squashfs and ISO assembly — roughly two
minutes instead of ten. It is a developer shortcut and must never be used for a
release: the image it produces reflects the package state of whenever that
rootfs was built.

---

## UEFI only

**Phase 1 is UEFI-only.** There is no BIOS or isolinux path anywhere — not on
the ISO, not on the installed system. The ISO is a GPT hybrid with the EFI
system partition appended as a real partition, so it boots from optical media
and from a plain `dd` to a USB stick, but only on UEFI firmware. The installer
refuses to run if `/sys/firmware/efi` is absent rather than installing a system
that cannot boot.

---

## What the image is

Debian trixie, `--variant=minbase`, plus exactly what
[`packages.list`](build/mkimage/packages.list) names. No metapackages, and
recommends are disabled image-wide through
`/etc/apt/apt.conf.d/99-nightshade` — which stays on the installed system, so
the manifest keeps meaning something after first boot.

Documentation, locales and the apt cache are stripped, and `dpkg` path-exclude
rules in `/etc/dpkg/dpkg.cfg.d/01-nightshade-nodoc` keep them out of anything
installed later. `/usr/share/doc/*/copyright` is deliberately kept: removing
license texts from a redistributed image would breach the terms of most of
what is in it.

Roughly 264 packages, a ~480 MB rootfs, a ~210 MB squashfs and a ~283 MB ISO.

### ZFS without a compiler

The image ships no compiler, no DKMS and no kernel headers, but it does ship a
working ZFS module. The build:

1. installs the toolchain and headers in the chroot and builds `zfs-dkms`
   against the exact kernel in the image (hook 0300),
2. gates on `modinfo zfs` actually resolving,
3. packages the built modules as **`nightshade-zfs-modules`**, a real `.deb`
   that declares `Provides: zfs-modules`,
4. purges the entire toolchain (hook 0900),
5. rebuilds the initramfs afterwards and gates on `sbin/zpool`, `sbin/zfs`,
   `scripts/local-top/zfs` and `zfs.ko` all being inside it (hook 0950).

Step 3 is load-bearing. `zfs-initramfs` depends on `zfs-modules | zfs-dkms`,
so purging `zfs-dkms` on its own makes apt remove `zfs-initramfs` too — and an
image without it boots to `ALERT! ZFS=rpool/ROOT/nightshade does not exist`.
`zfs-modules` is a virtual package that exists precisely so prebuilt module
packages can satisfy it.

Because nothing can rebuild the module, **the kernel is pinned** in
`/etc/apt/preferences.d/10-nightshade-kernel-pin`. Kernel updates are an
image-replacement operation, not an `apt upgrade`. To move the kernel by hand:
delete the pin, reinstall `zfs-dkms`, `linux-headers-amd64` and
`build-essential`, upgrade, and confirm `modinfo zfs` before rebooting.

### Other image decisions

- **SSH host keys are not baked in.** The `openssh-server` postinst generates
  them in the build chroot; shipping those would give every Nightshade box on
  earth the same host keys. They are deleted, and
  `nightshade-ssh-keygen.service` regenerates them per-machine on first boot.
  sshd itself is disabled by default.
- **`/etc/machine-id` is empty**, so systemd generates a fresh one per machine
  instead of every install sharing a DHCP identity and journal namespace.

---

## The live environment

The ISO boots to tty1, auto-logs in as root and launches
`/usr/local/bin/nightshade-installer` from `nightshade-installer.service`. If the
installer exits — for any reason — the session prints how to relaunch it and
drops to a root shell rather than leaving a dead console.

Root is also auto-logged-in on `ttyS0`. That is what makes the ISO testable
headlessly and gives remote hands a console on a machine with no video. It is
live-only.

Everything live-only is listed in `/usr/share/nightshade/live-manifest`, and
the installer reads that file when stripping the live layer out of the target.
There is one definition of "live-only" and it lives next to the hook that
creates those files.

---

## The installer

`src/nightshade-installer/` — a zero-dependency Rust crate. The engine
(`engine/`) owns the flow and every destructive action; frontends (`ui/`) only
ask questions and draw. That split is what lets the plain line-based flow and
the TUI be genuinely interchangeable.

Every system tool is driven through `cmd::Cmd`, which logs the argv, captures
both streams and returns errors carrying the command and its output. Nothing
about partitioning or ZFS is reimplemented in Rust.

### Flow

1. **Welcome** — branding and the destructive-operation warning.
2. **Disk selection** — enumerated from `/sys/block`. Virtual devices are
   skipped, and the live medium is excluded by resolving the mounted squashfs
   back through its loop device to the disk that backs it. Shows model, size,
   serial, media type and existing partitions. One disk, or exactly two for a
   mirror; a size difference over 10% warns but is allowed.
3. **Destruction confirmation** — lists exactly what will be destroyed and
   requires the literal word `ERASE`. No default-yes anywhere on this screen.
4. **User setup** — `nightshade`, password entered twice, minimum 8 characters,
   no empty password and no skip path. Root is locked; sudo comes from
   `/etc/sudoers.d/nightshade` (mode 0440, validated with `visudo`) and
   requires a password.
5. **Hostname** — validated against RFC 1123.
6. **Summary** — final confirmation.
7. **Install** — a streamed step log. On failure it shows the failing command
   and its output, and writes `/tmp/nightshade-install.log`.
8. **Done** — reboot or drop to a shell.

The password reaches the system exactly once, as stdin to `chpasswd` in the
target chroot. It is never in argv (world-readable via `/proc`), never in a
temporary file, and never rendered by `Debug`.

### On-disk layout

Per selected disk, GPT:

| part | size    | type | purpose                       |
|------|---------|------|-------------------------------|
| p1   | 512 MiB | EF00 | EFI system partition, FAT32   |
| p2   | 2 GiB   | BE00 | `bpool` member                |
| p3   | rest    | BF00 | `rpool` member                |

Two pools, because GRUB's ZFS reader only implements a subset of pool features:

- **`bpool`** — `compatibility=grub2`, `ashift=12`, `compression=lz4`
  (`zstd_compress` is not in the grub2 feature set), `canmount=off`,
  `bpool/BOOT/nightshade` mounted at `/boot`.
- **`rpool`** — `ashift=12`, `compression=zstd`, `acltype=posixacl`,
  `xattr=sa`, `atime=off`, `rpool/ROOT/nightshade` at `/`, with separate
  datasets for `/var/log` and `/var/tmp`.

Two disks make both pools mirror vdevs. Pools are always created against
`/dev/disk/by-id/` paths — a mirror built on `/dev/sda` and `/dev/sdb` works
right up until the kernel enumerates them the other way round.

The target rootfs is a copy of the running squashfs, not a second debootstrap.
One source of truth for the package set: what was tested when the ISO booted is
exactly what lands on disk, and an install cannot pull a newer package than the
image was gated against.

### Mirror boot

Both ESPs are made bootable. For each disk the installer runs `grub-install`
twice: once with `--bootloader-id` to create a named NVRAM entry, and once with
`--removable` to write the `EFI/BOOT/BOOTX64.EFI` fallback path. The fallback
is what actually makes a degraded mirror boot — if the disk whose NVRAM entry
the firmware prefers is gone, firmware falls back to the removable path on the
next device it finds, but only if one is there.

`/usr/local/sbin/nightshade-sync-esp` plus `nightshade-sync-esp.path` mirror
the primary ESP onto the second disk whenever `/boot/efi` changes, and once at
boot to catch drift from any period when the second disk was absent. Without
it the second ESP is a snapshot of installation day, and the first time it is
needed it boots a bootloader that no longer matches the system.

`/etc/fstab` carries only the ESPs — ZFS datasets mount from their own
`mountpoint` properties via `zfs-import-cache` and `zfs-mount`. The primary ESP
is `nofail` so a dead first disk cannot drop the machine into emergency mode
when it could have booted perfectly well off the survivor.

### Network configuration

An installed box comes up with **no addresses until something is configured**.
There is no DHCP client and no `ifupdown`; `systemd-networkd` is enabled and
has nothing to do until configd writes it some files.

That is the right default for a firewall whose interface assignment is a policy
decision. Configuring it is what the configuration system is for.

`systemd-resolved` is not installed, so `/etc/resolv.conf` is owned outright by
the system renderer, header and all — but only once something has been
committed. A box with nothing saved has the file the image gave it.

---

## The configuration system

Three programs and a schema.

**`schema/`** is YAML, and is the only place a configuration node is defined.
The validators, the defaults, the `?` help, the tab-completion tables and the
CLI's command tree all come from it. `build.rs` compiles it into Rust at build
time, so a schema that does not load fails the build rather than the box, and
nothing on the appliance goes looking for it on disk. A test asserts the
compiled schema and the source files describe the same tree.

**`nightshade-configd`** owns everything: validation, the candidate/running/
saved states, commit, rollback, rendering and applying. It runs as root from a
socket-activated unit and authenticates every connection with `SO_PEERCRED`,
recording the uid as the actor on every change.

**`ns`** is a thin client and a login shell. It edits nothing, applies nothing
and validates nothing — it turns a typed line into a request and prints the
answer, with configd's error text passed through verbatim.

### The config file

`/etc/nightshade/config.boot` is a VyOS/JunOS-style curly-brace document:

```
system {
    host-name fw-01
    name-server 1.1.1.1
}
interfaces {
    /* the uplink */
    ethernet eth0 {
        address 192.168.1.1/24
        mtu 9000
    }
}
```

Hand-editable by design, so it gets a real parser: strict grammar, a line and a
column on every error, comments carried into the tree, and
`parse(render(tree)) == tree` as a property test and a fuzz target.

### Commit

```
validate -> constraints -> diff -> order -> render -> check -> apply -> verify -> promote
```

Everything knowable without touching the machine is decided first, so the
common failures cost an error message rather than a half-configured firewall.
A failed apply restores the previous rendered artifacts; a failed restore says
so in as many words, because at that point the box matches no configuration at
all.

`commit confirm 5` applies the change and arms a rollback timer **inside
configd**, with a marker file recording the configuration to go back to. Losing
the session rolls it back; so does configd restarting, or being down when the
deadline passes.

### On boot

`config.boot` is parsed, validated, rendered and applied. If any of that fails
the box comes up on schema defaults — deliberately *without* a network — and
the reason is written where `ns` prints it before the first prompt. A firewall
whose policy failed to load must not bring up addresses on the interfaces whose
trust level is exactly what did not load.

### Using it

```
nightshade@nightshade> configure
nightshade@nightshade# set interfaces ethernet eth0 address 192.168.1.1/24
nightshade@nightshade# compare
nightshade@nightshade# commit confirm 5
nightshade@nightshade# confirm
nightshade@nightshade# save
```

`?` lists what can go here, `<Tab>` completes it. `| match`, `| count`,
`| no-more` and `| display json` are post-processing inside `ns` — not pipes,
and nothing is spawned. `shell` from operational mode drops to bash as your own
uid, gated and logged by configd; there is no other way to a shell, and
[a test audits the source](src/nightshade-cli/tests/no_shell_escapes.rs) to
keep it that way.

Non-interactively: `ns -c "show interfaces" --json`, or `ns -f batch-file`.
Exit codes are 0 for success, 1 for a command error, 2 for a configuration one.

### Looking at the interfaces

`show interfaces` is the operational command with the most in it, and its
output is Arista EOS's — the same columns, the same field names, the same
section order — so that eyes trained on one appliance read this one without
relearning. Two things are deliberately not EOS's: interfaces are called what
Linux calls them (`eth0`, never `Ethernet1`), and MAC addresses are written
`2c:dd:e9:12:00:a1` rather than in dotted quads.

```
show interfaces [<name>]                 the long form, per interface
show interfaces description              name, state and description
show interfaces status [<filter>]        port, speed, duplex, media
show interfaces counters [errors|discards|rates|queue|bins]
show interfaces transceiver [detail|properties|eeprom]
show interfaces capabilities | flowcontrol
show interfaces negotiation [detail]
show interfaces phy [detail]
show interfaces mac [detail]
clear counters [<name>]
```

Every one of them takes an interface or a range of them first —
`show interfaces eth0-3 counters errors` — and every one of them accepts
`| display json`, which emits the same data model the text is rendered from
rather than a second implementation of the command.

Counters, rates, link-flap counts and uptimes come from a sampler inside
configd rather than from reading the kernel when the command is typed: a rate
is two counters and the time between them, and a flap that happened while
nobody was looking still happened. The layouts are held to
[byte-exact fixtures](src/nightshade-ifstate/tests/golden/), and the
specification they implement is
[docs/specs/show-interfaces.md](docs/specs/show-interfaces.md).

---

## Testing

```sh
make test                         # the whole workspace
make test-vm                      # boot the ISO, two blank disks
make test-vm-disk                 # boot the installed system
make test-vm-degraded             # boot with disk 1 detached
make test-vm-degraded DEGRADE=0   # boot with disk 0 detached
make vm-reset                     # wipe VM disks and UEFI vars
```

`test-vm` defaults to a serial console (`VM_DISPLAY=none`), which is scriptable
and works over SSH. `VM_DISPLAY=gtk` gives a window.

---

## CI

[`.github/workflows/iso.yml`](.github/workflows/iso.yml) builds the installer
and the ISO inside a `debian:trixie` container. Debian packages and the cargo
registry are cached between runs.

The ISO, its checksum and its package manifest are uploaded on **every** run —
kept 90 days for tags, 14 for branch builds, since an untagged ISO is ~285MB of
throwaway. Artifact upload is set to fail on an empty match, so a build that
produces nothing fails loudly instead of succeeding with an empty artifact.

The container needs **`--privileged`**: `debootstrap` creates device nodes with
`mknod`, and the build bind-mounts `/proc`, `/sys` and `/dev` into the chroot.
A finer-grained equivalent is
`--cap-add=SYS_ADMIN --cap-add=MKNOD --security-opt apparmor=unconfined`.

Building inside trixie is not just tidiness. The installer binary is compiled
in CI but has to run inside the image, so it must link against trixie's glibc
rather than the runner's newer one. `mkimage.sh` enforces this by executing the
built binary inside the chroot before sealing the image, so a build on the
wrong base fails loudly instead of shipping a binary that dies at runtime on a
console with nothing else on it.

---

## Reproducibility

`SOURCE_DATE_EPOCH` is honoured throughout — `mksquashfs` reads it directly and
`xorriso` is passed a matching `--modification-date`. CI sets it to the commit
date. The package manifest of every build is recorded at
`/.disk/packages.txt` on the ISO and uploaded as a CI artifact, so two ISOs can
be compared package-for-package.
