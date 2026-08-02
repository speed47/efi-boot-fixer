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

# Framebuffer size, WxH. Delivered as the EDID preferred mode of the
# emulated VGA adapter, which is how OVMF can be talked into a resolution
# outside its built-in table: the video PCDs are fixed at build time and
# ignore fw_cfg. Setting a portrait shape — 800x1280 is the Steam Deck's
# panel exactly — is what makes the graphical backend rotate, and so the
# only way to exercise that path anywhere but on the hardware itself.
# Empty leaves OVMF's default. RES=none removes the video adapter entirely,
# which leaves the firmware publishing no graphics protocol at all and is
# how the fall back to its text console gets tested.
RES=${RES:-}
# Directory for periodic screendumps. The graphical backend writes nothing
# to the serial console, so this is the only way to see what it drew.
SHOTS=${SHOTS:-}
SHOT_EVERY=${SHOT_EVERY:-6}

# What the run should have done to the test disk.
#
# Without this the harness has no failure signal at all. QEMU is killed by
# `timeout` on every run, successful or not — it does not exit when the
# keypress feeder closes its stdin — and the application returns to the
# firmware rather than reporting anything back to us. So a run in which the
# application never started, or in which the keypresses landed in OVMF's
# boot manager instead of our menus, looks exactly like a run that worked.
# The disk is the only witness.
#
# `auto` works it out from the script and the corruption mkimages recorded.
# Set EXPECT=change|no-change|skip to state it directly.
EXPECT=${EXPECT:-auto}

CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
VARS_SRC=/usr/share/OVMF/OVMF_VARS_4M.fd
VARS="$DIR/vars.fd"
cp -f "$VARS_SRC" "$VARS"

# What mkimages did to test.img, if it left a note.
CORRUPTION=$(cat "$DIR/corruption" 2>/dev/null || true)

expected_effect() {
    case "$EXPECT" in
        change|no-change|skip) echo "$EXPECT"; return ;;
        auto) ;;
        *) echo "unknown EXPECT: $EXPECT (want change, no-change, skip or auto)" >&2
           exit 1 ;;
    esac

    local want
    case "$SCRIPT" in
        # Read-only walks, the run that declines at the gate, and the two
        # backup runs — those write a file to the ESP on the boot disk,
        # never to the disk being backed up.
        none|check|menu|display|inspect|scroll|repair-cancel|backup|backup-twice|bootentries)
            want=no-change ;;
        # Lowering FirstUsableLBA is what Prevent does to a *healthy*
        # table, and these images are built with FirstUsableLBA 2048 above
        # a 32-block entry array, so there is always a gap to close.
        prevent) want=change ;;
        repair)
            # Only 'bad-mbr' reaches our write path; see the note in
            # mkimages.sh. Everything else is either repaired by the
            # firmware before we run, refused outright, or already healthy.
            case "$CORRUPTION" in
                bad-mbr) want=change ;;
                *)       want=no-change ;;
            esac ;;
        # repair-boot targets disk 1, which mkimages never corrupts, and
        # restore needs a snapshot that some earlier run left on the ESP.
        # Neither has a post-condition this script can state on its own.
        *) want=skip ;;
    esac

    # EDK II's PartitionDxe rebuilds an invalid primary GPT from the backup
    # when it connects the disk, before any application runs. Under those
    # corruptions the image changes whatever we do, so "nothing was
    # written" stops being an observation this script can make.
    if [ "$want" = no-change ]; then
        case "$CORRUPTION" in
            zero-header|zero-all|bad-crc) want=skip ;;
        esac
    fi
    echo "$want"
}

# A cheap witness for "did anything we care about change".
#
# The application only ever writes the protective MBR, the two GPT headers
# and the two entry arrays, and all of those live in the first and last few
# dozen blocks. Hashing 64 GiB of mostly-sparse image to find that out
# would be absurd, and would also pick up filesystem churn we do not care
# about.
disk_digest() {
    local img=$1 blocks
    blocks=$(( $(stat -c%s "$img") / 512 ))
    {
        dd if="$img" bs=512 count=34 2>/dev/null
        dd if="$img" bs=512 skip=$(( blocks - 34 )) count=34 2>/dev/null
    } | sha256sum | cut -d' ' -f1
}

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
        scroll)                                     # page through a long report
            # Wants snapshots on the ESP already: run 'backup-twice' first
            # against the same images, without rebuilding them.
            keys "$DOWN" "$DOWN" "$DOWN" "$A"       # Restore GPT, snapshot list
            keys "$TAB"                             # View the whole record
            keys "$DOWN" "$DOWN" "$DOWN"            # three lines per press
            keys "$RIGHT"                           # then a whole screen
            keys "$UP" "$LEFT"                      # and back again
            keys "$A" "$B" "$B" ;;
        backup-twice)                               # two snapshots in a row
            keys "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            keys "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            keys "$B" ;;
        prevent)                                    # item 5
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            confirm
            keys "$A" "$B" ;;
        bootentries)                                # item 6, both read-only screens
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A"
            keys "$A"                               # View the boot entries
            keys "$DOWN" "$DOWN"                    # scroll it
            keys "$A"                               # dismiss, back to submenu
            keys "$DOWN" "$A"                       # Scan the ESPs
            keys "$DOWN" "$A"                       # scroll, dismiss
            keys "$B" "$B" ;;
        menu)                                       # walk the menu, choose nothing
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$UP" "$UP" "$B" ;;
        display)                                    # the startup display screen
            # Only reached with a portrait RES, and only if the first key
            # lands while it is still up -- it continues on its own after
            # six seconds. Tune BOOT_WAIT, not this.
            keys "$DOWN" "$DOWN" "$UP"              # text size down, down, up
            keys "$LEFT" "$RIGHT"                   # turn away and back
            keys "$A" "$B" ;;                       # accept, then exit
        *)
            echo "unknown script: $SCRIPT" >&2; exit 1 ;;
    esac
    sleep 8
}

EXTRA=()
case "$RES" in
    "")   ;;
    none) EXTRA+=(-vga none) ;;
    *)    EXTRA+=(-vga none -device "VGA,edid=on,xres=${RES%x*},yres=${RES#*x}") ;;
esac
if [ -n "$SHOTS" ]; then
    rm -rf "$SHOTS"; mkdir -p "$SHOTS"
    EXTRA+=(-qmp "unix:$DIR/qmp.sock,server,nowait")
    # Started now so it is already waiting on the socket QEMU is about to
    # create. It gives up on its own when QEMU goes away.
    "$(dirname "$0")/qemu-shots.py" "$DIR/qmp.sock" "$SHOTS" "$SHOT_EVERY" \
        "$(( TIMEOUT / SHOT_EVERY ))" &
fi

EFFECT=$(expected_effect)
BEFORE=$(disk_digest "$DIR/test.img")

set +e
drive | timeout "$TIMEOUT" qemu-system-x86_64 \
    -machine q35 \
    -m 512 \
    "${EXTRA[@]}" \
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
# Not a failure in itself: qemu never exits on its own once the keypresses
# are done, so `timeout` kills it on every run and 124 is the normal code.
echo "### qemu exited with $rc (script: $SCRIPT) ###"

AFTER=$(disk_digest "$DIR/test.img")
case "$EFFECT" in
    skip)
        echo "### test disk: no post-condition for '$SCRIPT' with corruption" \
             "'${CORRUPTION:-unknown}', not checked ###" ;;
    change)
        if [ "$BEFORE" = "$AFTER" ]; then
            echo "### FAILED: '$SCRIPT' left the test disk untouched ###" >&2
            echo "It was supposed to write to it. Either the application never" >&2
            echo "started, or the keypresses landed somewhere other than its" >&2
            echo "menus -- raise BOOT_WAIT if this host is slow." >&2
            exit 1
        fi
        echo "### test disk changed, as '$SCRIPT' should have ###" ;;
    no-change)
        if [ "$BEFORE" != "$AFTER" ]; then
            echo "### FAILED: '$SCRIPT' wrote to the test disk ###" >&2
            echo "Nothing in this run was supposed to touch it." >&2
            exit 1
        fi
        echo "### test disk untouched, as '$SCRIPT' should have left it ###" ;;
esac
