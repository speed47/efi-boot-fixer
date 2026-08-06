#!/usr/bin/env bash
# Run every walk run-qemu.sh knows against fresh QEMU images, and report
# which ones verified. This is the QEMU-backed half of `make check` -- see
# docs/testing.md for what each walk proves and why OVMF makes some
# corruption modes untestable this way.
#
# Most walks want images no earlier walk has touched: a write another walk
# made to test.img, or a boot-*.bkp another walk left on the ESP, would
# change what a fresh run's post-condition actually proves. So this rebuilds
# the images before each of those. A handful of walks are deliberately
# paired and share one build instead, because the second half only proves
# anything if it inherits what the first half left behind -- see the
# comments in run-qemu.sh next to backup-usb-only, scroll and bootrestore.
#
# On slow (e.g. TCG-emulated) hosts, raise BOOT_WAIT/STEP/TIMEOUT as you
# would for a single run-qemu.sh call; they are honoured here the same way,
# since each walk below just shells out to run-qemu.sh itself.
#
# -e stays on: a walk failing is caught by walk()'s own `if`, which -e does
# not trip on, but a failed `fresh` (mkimages.sh itself broke) has nothing
# sensible to continue into and should stop the whole run.
set -euo pipefail

IMAGES=${1:?usage: qemu-test-all.sh <image-dir> <path-to-efi>}
EFI=${2:?usage: qemu-test-all.sh <image-dir> <path-to-efi>}
CORRUPTION=${CORRUPTION:-bad-mbr}

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN="$HERE/run-qemu.sh"
MKIMAGES="$HERE/mkimages.sh"

TOTAL=0
FAILED=()

fresh() {
    echo "--- building images (corruption=$CORRUPTION) ---"
    "$MKIMAGES" "$IMAGES" "$EFI" "$CORRUPTION" >/dev/null
}

# walk <label> <command...> - the command is whatever runs this walk,
# typically `"$RUN" "$IMAGES" <script>` with an `env FOO=bar` prefix for
# the walks that need one. Output streams straight to the console, same as
# calling run-qemu.sh by hand, so a failure's explanation is right there.
walk() {
    local label=$1; shift
    TOTAL=$((TOTAL + 1))
    echo
    echo "=== [$TOTAL] $label ==="
    if "$@"; then
        echo "--- $label: ok ---"
    else
        echo "--- $label: FAILED ---" >&2
        FAILED+=("$label")
    fi
}

# Solo walks: each wants pristine images, and none leaves anything the
# next one would care about.
for script in none overview report check backup prevent menu \
              bootentries bootnext bootdefault bootbackup repair-cancel repair-boot; do
    fresh
    walk "$script" "$RUN" "$IMAGES" "$script"
done

# Needs the test disk absent, not just unbroken -- the whole point is that
# the picker never appears.
fresh
walk "check-one" env ONE_DISK=1 "$RUN" "$IMAGES" check-one

# Wants the disk actually broken (bad-mbr, the only mode that reaches the
# write path under OVMF) to prove the repair changed it.
fresh
walk "repair" "$RUN" "$IMAGES" repair

# USB-only, each standalone.
fresh
walk "backup-usb" env USB=1 "$RUN" "$IMAGES" backup-usb
fresh
walk "report-usb" env USB=1 "$RUN" "$IMAGES" report-usb

# Paired: a snapshot on the stick only backup-usb-only leaves, read back by
# restore-usb from the very same images so it can only have come from there.
fresh
walk "backup-usb-only" env USB=1 "$RUN" "$IMAGES" backup-usb-only
walk "restore-usb" env USB=1 "$RUN" "$IMAGES" restore-usb

# Paired: two snapshots on the ESP, then browsed by inspect and paged by
# scroll from the same images -- see the comment above 'scroll' in
# run-qemu.sh.
fresh
walk "backup-twice" "$RUN" "$IMAGES" backup-twice
walk "inspect" "$RUN" "$IMAGES" inspect
walk "scroll" "$RUN" "$IMAGES" scroll

# restore has no post-condition of its own (run-qemu.sh has no way to name
# one), but it should still walk the menu a healthy backup would populate.
walk "restore" "$RUN" "$IMAGES" restore

# Paired: bootregister writes a boot-NNN.bkp that only bootrestore reads
# back, and only if the NVRAM it wrote is still there -- KEEP_VARS=1 keeps
# vars.fd instead of resetting it to OVMF's pristine store.
fresh
walk "bootregister" "$RUN" "$IMAGES" bootregister
walk "bootrestore" env KEEP_VARS=1 "$RUN" "$IMAGES" bootrestore

# The graphical screens need a real framebuffer. 800x600 is smaller than
# what OVMF's own mode list otherwise offers, which is what gives
# display-mode and display-revert a bigger mode to pick from the resolution
# menu and either keep or let lapse.
fresh
walk "display" env RES=800x1280 "$RUN" "$IMAGES" display
fresh
walk "display-mode" env RES=800x600 "$RUN" "$IMAGES" display-mode
fresh
walk "display-revert" env RES=800x600 "$RUN" "$IMAGES" display-revert

echo
echo "### ran $TOTAL walks, ${#FAILED[@]} failed ###"
if [ "${#FAILED[@]}" -gt 0 ]; then
    printf 'FAILED: %s\n' "${FAILED[@]}" >&2
    exit 1
fi
echo "### all qemu walks verified ###"
