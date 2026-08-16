# Nightshade build entrypoints.
#
# Everything here assumes a Linux host. On Windows, drive it through WSL:
#   wsl -d Ubuntu -u root make iso
#
# The ISO build needs root (debootstrap, chroot, bind mounts). Targets that
# need it pick up $(SUDO) automatically.

# The VERSION file at the repo root is the single source of truth. CI checks a
# release tag against it, so `git tag v0.2.0` without updating VERSION fails
# rather than shipping an ISO whose name disagrees with its os-release.
VERSION      ?= $(shell tr -d '[:space:]' <VERSION)
ifeq ($(strip $(VERSION)),)
$(error VERSION is empty or the VERSION file is missing from the repo root)
endif
ARCH         := amd64

DIST         ?= dist

# Timestamps and the stamp in the ISO name both come from SOURCE_DATE_EPOCH, so
# rebuilding the same commit reproduces the same filename as well as the same
# bytes. Left unset, it is simply now.
ifeq ($(origin SOURCE_DATE_EPOCH), undefined)
SOURCE_DATE_EPOCH := $(shell date -u +%s)
endif
export SOURCE_DATE_EPOCH

# YYYYMMDDHHMM, UTC. Minutes matter: several iterations of the same version in
# one afternoon is the normal case while developing, and a date alone makes
# them all the same filename.
BUILD_STAMP  := $(shell date -u -d "@$(SOURCE_DATE_EPOCH)" +%Y%m%d%H%M)

# A release is nightshade-<version>.iso. Anything else carries the build stamp,
# because otherwise every development build of 0.1.0 has the same filename and
# there is no way to tell which one is on the USB stick.
RELEASE      ?= 0
ifeq ($(RELEASE),1)
ISO_NAME     := nightshade-$(VERSION)
else
ISO_NAME     := nightshade-$(VERSION)-$(BUILD_STAMP)
endif
ISO          := $(DIST)/$(ISO_NAME).iso

# Work tree and VM state default to a native Linux filesystem. Under WSL the
# repo itself often lives on /mnt/c (9p), which cannot hold a rootfs and is slow
# for multi-gigabyte disk images.
WORKDIR      ?= /var/tmp/nightshade-build
VM_DIR       ?= /var/tmp/nightshade-vm
APT_CACHE    ?=

# Overridable so the build can put target/ on a native filesystem. Under WSL a
# cargo target dir on /mnt/c is dramatically slower than one on ext4.
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR
INSTALLER    := $(CARGO_TARGET_DIR)/release/nightshade-installer

# VM settings
VM_MEM       ?= 4096
VM_CPUS      ?= 4
VM_DISK_SIZE ?= 20G
OVMF_CODE    ?= /usr/share/OVMF/OVMF_CODE_4M.fd
OVMF_VARS    ?= /usr/share/OVMF/OVMF_VARS_4M.fd
# `none` runs headless on the serial console, which is scriptable and works over
# SSH. Set VM_DISPLAY=gtk for a window (WSLg or a real X/Wayland session).
VM_DISPLAY   ?= none

SUDO         := $(shell [ "$$(id -u)" -eq 0 ] || echo sudo)

MKIMAGE_ARGS := -o $(ISO) -v $(VERSION) -w $(WORKDIR)
ifneq ($(APT_CACHE),)
MKIMAGE_ARGS += -c $(APT_CACHE)
endif

QEMU_COMMON = \
	-machine q35,accel=kvm:tcg \
	-cpu host \
	-smp $(VM_CPUS) \
	-m $(VM_MEM) \
	-drive if=pflash,format=raw,unit=0,readonly=on,file=$(OVMF_CODE) \
	-drive if=pflash,format=raw,unit=1,file=$(VM_DIR)/OVMF_VARS.fd \
	-netdev user,id=net0 -device virtio-net-pci,netdev=net0

ifeq ($(VM_DISPLAY),none)
QEMU_DISPLAY = -nographic
else
QEMU_DISPLAY = -display $(VM_DISPLAY)
endif

.PHONY: all iso installer test-vm test-vm-disk test-vm-degraded vm-reset clean help

all: iso

help:
	@echo "Nightshade $(VERSION)"
	@echo
	@echo "  make installer        build the Rust installer (release)"
	@echo "  make iso              build $(ISO)"
	@echo "  make iso RELEASE=1    build $(DIST)/nightshade-$(VERSION).iso (no stamp)"
	@echo "  make test-vm          boot the ISO in QEMU/OVMF with two blank $(VM_DISK_SIZE) disks"
	@echo "  make test-vm-disk     boot the installed system from those disks"
	@echo "  make test-vm-degraded boot with disk 1 detached (mirror degradation test)"
	@echo "  make vm-reset         wipe VM disks and UEFI vars"
	@echo "  make clean            remove dist/, target/ and the build work tree"
	@echo
	@echo "  VM_DISPLAY=gtk make test-vm    run with a window instead of serial"

# ---------------------------------------------------------------------------
# installer
# ---------------------------------------------------------------------------

# Scoped to the one crate on purpose. The workspace now also holds configd, the
# CLI and their dependency trees; building all of it under the release profile
# (LTO, one codegen unit) to produce a binary the ISO does not yet contain adds
# minutes to every `make iso`.
installer:
	cargo build --release --locked -p nightshade-installer
	@ls -l $(INSTALLER)

$(INSTALLER): installer

# ---------------------------------------------------------------------------
# iso
# ---------------------------------------------------------------------------

iso: $(INSTALLER)
	$(SUDO) build/mkimage/mkimage.sh $(MKIMAGE_ARGS) -b $(INSTALLER)

# Build the ISO without the Rust crate. The live session comes up and drops to a
# shell instead of running an installer; useful for iterating on the image
# pipeline itself.
.PHONY: iso-noinstaller
iso-noinstaller:
	$(SUDO) build/mkimage/mkimage.sh $(MKIMAGE_ARGS)

# ---------------------------------------------------------------------------
# QEMU
# ---------------------------------------------------------------------------

# Blank disks and a pristine UEFI variable store. OVMF_VARS must be a writable
# per-VM copy; sharing the system one would have QEMU fail or, worse, persist
# boot entries between unrelated test runs.
$(VM_DIR)/OVMF_VARS.fd:
	@mkdir -p $(VM_DIR)
	cp $(OVMF_VARS) $@
	chmod u+w $@

$(VM_DIR)/disk0.qcow2:
	@mkdir -p $(VM_DIR)
	qemu-img create -f qcow2 $@ $(VM_DISK_SIZE)

$(VM_DIR)/disk1.qcow2:
	@mkdir -p $(VM_DIR)
	qemu-img create -f qcow2 $@ $(VM_DISK_SIZE)

VM_DISKS = $(VM_DIR)/disk0.qcow2 $(VM_DIR)/disk1.qcow2 $(VM_DIR)/OVMF_VARS.fd

# Serials are what the installer shows in the disk picker, so give them values
# that make the two disks tellable apart on screen.
QEMU_DISK0 = -drive file=$(VM_DIR)/disk0.qcow2,if=none,id=d0,format=qcow2 \
             -device virtio-blk-pci,drive=d0,serial=NSDISK0
QEMU_DISK1 = -drive file=$(VM_DIR)/disk1.qcow2,if=none,id=d1,format=qcow2 \
             -device virtio-blk-pci,drive=d1,serial=NSDISK1

test-vm: $(VM_DISKS)
	@echo "==> booting $(ISO) (ctrl-a x to quit)"
	qemu-system-x86_64 $(QEMU_COMMON) $(QEMU_DISPLAY) \
		-drive file=$(ISO),if=none,id=iso,format=raw,media=cdrom \
		-device ide-cd,drive=iso,bootindex=0 \
		$(QEMU_DISK0) $(QEMU_DISK1)

test-vm-disk: $(VM_DISKS)
	@echo "==> booting installed system from disk (ctrl-a x to quit)"
	qemu-system-x86_64 $(QEMU_COMMON) $(QEMU_DISPLAY) \
		$(QEMU_DISK0) $(QEMU_DISK1)

# Acceptance test: a two-disk mirror must still boot with one member absent.
# DEGRADE=0 removes disk0 instead of disk1.
DEGRADE ?= 1
test-vm-degraded: $(VM_DISKS)
	@echo "==> booting with disk$(DEGRADE) detached (ctrl-a x to quit)"
	qemu-system-x86_64 $(QEMU_COMMON) $(QEMU_DISPLAY) \
		$(if $(filter 0,$(DEGRADE)),$(QEMU_DISK1),$(QEMU_DISK0))

vm-reset:
	rm -rf $(VM_DIR)

# ---------------------------------------------------------------------------
# housekeeping
# ---------------------------------------------------------------------------

# The whole workspace, and in the dev profile. The release profile's LTO and
# single codegen unit exist to make the shipped binaries small, which is not a
# property tests measure -- paying for it on every test run buys nothing.
.PHONY: test
test:
	cargo test --workspace --locked

# Named build products rather than the whole of dist/: dist/systemd and
# dist/completions are source, not output.
clean:
	rm -f $(DIST)/*.iso $(DIST)/*.iso.sha256 $(DIST)/*.iso.packages.txt
	rm -rf $(CARGO_TARGET_DIR)
	$(SUDO) rm -rf $(WORKDIR)
