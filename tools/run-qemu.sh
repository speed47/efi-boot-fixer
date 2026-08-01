#!/usr/bin/env bash
# Boot the application under OVMF with two NVMe disks and drive its menus.
#
#   boot.img - the ESP the image is launched from, disk 1 in the picker
#   test.img - a SteamOS-shaped disk with a deliberate defect, disk 2
#
# The application no longer excludes the disk it booted from, so both are
# offered; the scripts below select disk 2 by pressing DOWN once.
#
# Input arrives over the serial console, where OVMF's TerminalDxe turns
# ANSI sequences into scan codes: ESC[A..D become the D-pad, CR is A, and a
# lone ESC is B. That is the same alphabet the Deck's buttons produce, so
# these runs exercise the real code path rather than a keyboard-only one.
#
# On a non-x86 host this runs under TCG emulation, so allow a few minutes.
set -euo pipefail

DIR=${1:?usage: run-qemu.sh <image-dir> [script]}
SCRIPT=${2:-none}
TIMEOUT=${TIMEOUT:-420}
# How long to wait before the first keypress. The menus drain queued input
# on entry — a burst of auto-repeat must not walk through a confirmation —
# so keys pressed before the application starts are discarded, and this has
# to be long enough for OVMF to reach it.
BOOT_WAIT=${BOOT_WAIT:-105}
# Gap between keypresses, long enough for a screen to repaint under TCG.
STEP=${STEP:-3}

CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
VARS_SRC=/usr/share/OVMF/OVMF_VARS_4M.fd
VARS="$DIR/vars.fd"
cp -f "$VARS_SRC" "$VARS"

UP=$'\033[A'; DOWN=$'\033[B'; RIGHT=$'\033[C'; LEFT=$'\033[D'
A=$'\r'; B=$'\033'; TAB=$'\t'

keys() {
    local k
    for k in "$@"; do
        printf '%s' "$k"
        sleep "$STEP"
    done
}

# The five-press gate: LEFT RIGHT LEFT RIGHT A.
confirm() { keys "$LEFT" "$RIGHT" "$LEFT" "$RIGHT" "$A"; }

drive() {
    sleep "$BOOT_WAIT"
    case "$SCRIPT" in
        none)                                       # look at the menu, leave
            keys "$B" ;;
        check)                                      # item 1, disk 2, page, exit
            keys "$A" "$DOWN" "$A" "$A" "$B" ;;
        repair)                                     # item 2
            keys "$DOWN" "$A" "$DOWN" "$A" "$A"
            confirm
            keys "$A" "$B" ;;
        repair-boot)                                # repair disk 1, the disk
            keys "$DOWN" "$A" "$A" "$A"             # this image booted from
            confirm
            keys "$A" "$B" ;;
        repair-cancel)                              # decline at the gate
            keys "$DOWN" "$A" "$DOWN" "$A" "$A" "$B" "$A" "$B" ;;
        backup)                                     # item 3
            keys "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A" "$B" ;;
        restore)                                    # item 4
            keys "$DOWN" "$DOWN" "$DOWN" "$A" "$A" "$DOWN" "$A" "$A"
            confirm
            keys "$A" "$B" ;;
        inspect)                                    # browse snapshots, View
            keys "$DOWN" "$DOWN" "$DOWN" "$A"       # Restore GPT
            keys "$DOWN"                            # second snapshot
            keys "$TAB" "$A"                        # details, then back
            keys "$DOWN" "$TAB" "$A"                # and the next one
            keys "$B" "$B" ;;
        backup-twice)                               # two snapshots in a row
            keys "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            keys "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            keys "$B" ;;
        prevent)                                    # item 5
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            confirm
            keys "$A" "$B" ;;
        menu)                                       # walk the menu, choose nothing
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$UP" "$UP" "$B" ;;
        *)
            echo "unknown script: $SCRIPT" >&2; exit 1 ;;
    esac
    sleep 8
}

set +e
drive | timeout "$TIMEOUT" qemu-system-x86_64 \
    -machine q35 \
    -m 512 \
    -drive if=pflash,format=raw,unit=0,readonly=on,file="$CODE" \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$DIR/boot.img",format=raw,if=none,id=bootdisk \
    -device nvme,drive=bootdisk,serial=BOOTDISK \
    -drive file="$DIR/test.img",format=raw,if=none,id=testdisk \
    -device nvme,drive=testdisk,serial=TESTDISK \
    -net none \
    -nographic
rc=$?
set -e
echo
echo "### qemu exited with $rc (script: $SCRIPT) ###"
