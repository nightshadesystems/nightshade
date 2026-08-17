#!/bin/sh
# Produce the ISO's boot artifacts using the chroot's own GRUB.
#
# Deliberately built inside the chroot rather than on the build host: the EFI
# binary and the theme fonts then come from exactly the GRUB version that the
# image ships, so the ISO does not depend on whatever grub-common the CI runner
# or a developer's laptop happens to have.
. /run/nightshade-build/hooks/lib.sh

OUT="$STAGING/out/grub"
mkdir -p "$OUT/fonts" "$OUT/efi/EFI/BOOT"

# --- fonts ----------------------------------------------------------------
DEJAVU=/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf
[ -f "$DEJAVU" ] || die "missing $DEJAVU (fonts-dejavu-core not installed?)"

log "rendering theme fonts"
# Renaming the family to "Nightshade Mono" decouples theme.txt from whichever
# TTF we happen to source; theme.txt never has to know it is DejaVu.
grub-mkfont --name="Nightshade Mono" --size=16 --output="$OUT/fonts/mono16.pf2" "$DEJAVU"
grub-mkfont --name="Nightshade Mono" --size=24 --output="$OUT/fonts/mono24.pf2" "$DEJAVU"
for f in mono16 mono24; do
    [ -s "$OUT/fonts/$f.pf2" ] || die "grub-mkfont produced an empty $f.pf2"
done

# Stock unifont: GRUB's fallback when a glyph is missing from the theme font.
if [ -f /usr/share/grub/unicode.pf2 ]; then
    cp /usr/share/grub/unicode.pf2 "$OUT/fonts/unicode.pf2"
fi

# --- theme ----------------------------------------------------------------
# GRUB's gfxmenu ignores width/height on an image component: it lays the bitmap
# out at its natural size (clamped to the space right of `left`) and scales to
# that. So the logo has to be delivered at exactly its display size, or it
# renders enormous and paints over the menu labels.
LOGO_W=440
LOGO_H=120
SRC_LOGO="$STAGING/grub/theme/nightshade-lockup-cropped-on-dark.png"
[ -f "$SRC_LOGO" ] || die "source logo missing at $SRC_LOGO"

log "assembling theme (logo -> ${LOGO_W}x${LOGO_H})"
mkdir -p "$OUT/theme"
cp -a "$STAGING/grub/theme/." "$OUT/theme/"

# The source lockup is never modified; this writes a derived asset alongside it.
pngtopnm "$SRC_LOGO" \
    | pamscale -width "$LOGO_W" -height "$LOGO_H" \
    | pnmtopng >"$OUT/theme/logo.png"
[ -s "$OUT/theme/logo.png" ] || die "failed to scale the logo"

# The oversized source has no business on the ISO or in /boot.
rm -f "$OUT/theme/$(basename "$SRC_LOGO")"

# A theme that names a file it does not ship renders as a blank purple screen
# with no menu, which is a miserable thing to discover on real hardware.
log "gate: theme references only files that exist"
sed -n 's/.*file[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$OUT/theme/theme.txt" \
    | sort -u \
    | while read -r ref; do
        [ -f "$OUT/theme/$ref" ] || { log "missing theme asset: $ref"; exit 1; }
      done \
    || die "theme.txt references an asset that is not present"

# The installed system gets the same theme and fonts; the installer copies these
# into the target's /boot/grub. Shipping them inside the image (rather than
# re-deriving them at install time) keeps the ISO menu and the installed menu
# byte-identical.
log "installing theme into the image for the installer to reuse"
rm -rf /usr/share/nightshade/grub-theme /usr/share/nightshade/grub-fonts
mkdir -p /usr/share/nightshade/grub-fonts
cp -a "$OUT/theme" /usr/share/nightshade/grub-theme
cp "$OUT/fonts"/*.pf2 /usr/share/nightshade/grub-fonts/
install -m 0644 "$STAGING/grub/default-grub.in" /usr/share/nightshade/default-grub.in
# The grub.d drop-in that actually turns the graphical menu on. Shipped as data
# rather than installed into /etc/grub.d here: this image never runs
# grub-mkconfig on itself, and a live ISO with a stray boot-menu generator in
# /etc/grub.d is a trap for anyone who runs update-grub while debugging.
install -m 0644 "$STAGING/grub/nightshade-gfx.in" /usr/share/nightshade/nightshade-gfx.in

# --- standalone EFI binary ------------------------------------------------
# The embedded config does nothing but find the ISO and hand over to the real
# menu, so the menu stays editable on the ISO filesystem instead of being frozen
# inside a memdisk.
EARLY="$STAGING/early-grub.cfg"
cat >"$EARLY" <<EOF
search --no-floppy --set=root --label $ISO_LABEL
if [ -z "\$root" ]; then
    search --no-floppy --set=root --file /boot/grub/grub.cfg
fi
set prefix=(\$root)/boot/grub
configfile (\$root)/boot/grub/grub.cfg
EOF

log "building BOOTX64.EFI"
grub-mkstandalone \
    --format=x86_64-efi \
    --output="$OUT/efi/EFI/BOOT/BOOTX64.EFI" \
    --locales="" \
    --themes="" \
    --modules="part_gpt part_msdos fat iso9660 normal linux boot configfile echo test search search_label search_fs_uuid search_fs_file all_video gfxterm gfxmenu png font loadenv videoinfo terminal minicmd reboot halt sleep serial gzio" \
    "boot/grub/grub.cfg=$EARLY"

[ -s "$OUT/efi/EFI/BOOT/BOOTX64.EFI" ] || die "grub-mkstandalone produced nothing"
EFI_BYTES=$(stat -c %s "$OUT/efi/EFI/BOOT/BOOTX64.EFI")
log "BOOTX64.EFI is $((EFI_BYTES / 1024)) KiB"

# --- EFI system partition image ------------------------------------------
# Built with mtools rather than by loop-mounting a filesystem, so this step
# needs no privileges and works unchanged in an unprivileged CI container.
#
# FAT16, not FAT32: the image is only a few MiB and FAT32 needs ~33 MiB before
# its cluster count is legal. Removable-media FAT16 ESPs are what Debian's own
# installer ISOs ship, and OVMF plus every UEFI implementation worth supporting
# reads them.
ESP_MB=$(( (EFI_BYTES / 1048576) + 5 ))
[ "$ESP_MB" -lt 8 ] && ESP_MB=8
ESP="$STAGING/out/esp.img"

log "creating ${ESP_MB}MiB FAT16 ESP image"
rm -f "$ESP"
truncate -s "${ESP_MB}M" "$ESP"
mformat -i "$ESP" -v NSESP ::
mmd -i "$ESP" ::/EFI ::/EFI/BOOT
mcopy -i "$ESP" "$OUT/efi/EFI/BOOT/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI

log "gate: reading BOOTX64.EFI back out of the ESP"
mdir -i "$ESP" ::/EFI/BOOT | grep -qi 'BOOTX64' \
    || die "BOOTX64.EFI is not readable inside the ESP image"

log "grub assets built"
