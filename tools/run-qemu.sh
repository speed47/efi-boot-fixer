#!/usr/bin/env bash
# Boot the application under OVMF with two NVMe disks and drive its menus.
#
#   boot.img - the ESP the image is launched from, disk 1 in the picker
#   test.img - a SteamOS-shaped disk with a deliberate defect, disk 2
#   usb.img  - a removable FAT volume, attached only when USB=1
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
# Each run normally starts from the firmware's pristine NVRAM, so a walk
# never inherits variables an earlier one wrote. KEEP_VARS=1 keeps the
# store from the previous run instead, which is how a boot entry written
# by one run is shown being read back by the next -- the only way to
# demonstrate that a variable really did survive a reboot.
if [ "${KEEP_VARS:-0}" != 1 ] || [ ! -f "$VARS" ]; then
    cp -f "$VARS_SRC" "$VARS"
fi

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
        # Read-only walks, the run that declines at the gate, and the backup
        # runs — those write a file to the ESP or to the stick, never to the
        # disk being backed up.
        none|overview|check|menu|display|inspect|scroll|repair-cancel|backup|backup-twice)
            want=no-change ;;
        display-mode|display-revert)
            want=no-change ;;
        backup-usb|backup-usb-only|bootentries|report|report-usb)
            want=no-change ;;
        # The test disk is not attached at all, so there is nothing to say
        # about it.
        check-one) want=skip ;;
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

    # EDK II's PartitionDxe rebuilds an invalid main GPT from the secondary
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

# Into the two submenus from the top level, which is where every operation
# now lives. The rows above them are "Check this machine" and "Save a
# diagnostic report", so the GPT submenu is the third row and the NVRAM one
# the fourth.
gpt_menu()   { keys "$DOWN" "$DOWN" "$A"; }
nvram_menu() { keys "$DOWN" "$DOWN" "$DOWN" "$A"; }

drive() {
    sleep "$BOOT_WAIT"
    case "$SCRIPT" in
        none)                                       # look at the menu, leave
            keys "$B" ;;
        overview)                                   # the read-only summary
            keys "$A"                               # Check this machine
            keys "$DOWN" "$DOWN"                    # scroll it
            keys "$A" "$B" ;;
        report)                                     # the diagnostic report
            # Second row of the main menu. The report is built, shown, then
            # saved: A continues past the report page, the destination menu
            # is only offered with USB=1 (see 'report-usb'), then A accepts
            # the review page and A dismisses the result.
            keys "$DOWN" "$A"                       # Generate a diagnostic report
            keys "$DOWN" "$DOWN" "$RIGHT"           # scroll it, then a screen
            keys "$A"                               # continue to saving
            keys "$A" "$A"                          # review page, result
            keys "$B" ;;
        report-usb)                                 # the same, to both volumes
            # Needs USB=1: the destination menu appears between the report
            # page and the review page, and its first row is "Save to both".
            # Without the stick that extra A would land on the result screen
            # and the run would end one screen short, which is the test.
            keys "$DOWN" "$A"
            keys "$A"                               # continue to saving
            keys "$A"                               # save to both
            keys "$A" "$A"                          # review page, result
            keys "$B" ;;
        check)                                      # GPT item 1, disk 2, page
            gpt_menu
            keys "$A" "$DOWN" "$A" "$A" "$B" "$B" ;;
        check-one)                                  # ONE_DISK=1: no picker
            # One press should land on the report itself. If the picker were
            # still offered, the second A would choose the disk and the run
            # would end one screen short of it -- so grep the serial log for
            # "Check GPT (read only)" to tell the two apart.
            gpt_menu
            keys "$A" "$A" "$B" "$B" ;;
        repair)                                     # GPT item 4
            gpt_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A"
            confirm
            keys "$A" "$B" "$B" ;;
        repair-boot)                                # repair disk 1, the disk
            gpt_menu                                # this image booted from
            keys "$DOWN" "$DOWN" "$DOWN" "$A" "$A" "$A"
            confirm
            keys "$A" "$B" "$B" ;;
        repair-cancel)                              # decline at the gate
            gpt_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$B" "$A" "$B" "$B" ;;
        backup)                                     # GPT item 2
            gpt_menu
            keys "$DOWN" "$A" "$DOWN" "$A" "$A" "$A" "$B" "$B" ;;
        backup-usb)                                 # to the ESP and the stick
            # Needs USB=1, which is what puts a destination menu between the
            # disk picker and the review page. Its first row is "Save to
            # both", so this is the plain 'backup' walk with one more A in
            # the middle -- and if the menu were somehow not offered, that
            # extra press would land on the result screen and the run would
            # end one screen short, which is what makes it a test.
            gpt_menu
            keys "$DOWN" "$A" "$DOWN" "$A" "$A" "$A" "$A" "$B" "$B" ;;
        backup-usb-only)                            # to the stick alone
            gpt_menu
            keys "$DOWN" "$A" "$DOWN" "$A"          # Back up, disk 2
            keys "$DOWN" "$DOWN" "$A"               # third row: the stick only
            keys "$A" "$A" "$B" "$B" ;;
        restore-usb)                                # from the copy on the stick
            # Wants exactly one snapshot, and it on the stick: rebuild the
            # images and run 'backup-usb-only' first. Then the single row
            # offered here can only have come from removable media, which is
            # the thing this walk is for -- and the "found on" line in the
            # detail pane says so.
            gpt_menu
            keys "$DOWN" "$DOWN" "$A"               # Restore GPTs
            keys "$A"                               # the only row
            keys "$DOWN" "$A" "$A"                  # disk 2, review page
            confirm
            keys "$A" "$B" "$B" ;;
        restore)                                    # GPT item 3
            gpt_menu
            keys "$DOWN" "$DOWN" "$A" "$A" "$DOWN" "$A" "$A"
            confirm
            keys "$A" "$B" "$B" ;;
        inspect)                                    # browse snapshots, View
            gpt_menu
            keys "$DOWN" "$DOWN" "$A"               # Restore GPTs
            keys "$DOWN"                            # second snapshot
            keys "$TAB" "$A"                        # details, then back
            keys "$DOWN" "$TAB" "$A"                # and the next one
            keys "$B" "$B" "$B" ;;
        scroll)                                     # page through a long report
            # Wants snapshots on the ESP already: run 'backup-twice' first
            # against the same images, without rebuilding them.
            gpt_menu
            keys "$DOWN" "$DOWN" "$A"               # Restore GPTs, snapshot list
            keys "$TAB"                             # View the whole record
            keys "$DOWN" "$DOWN" "$DOWN"            # three lines per press
            keys "$RIGHT"                           # then a whole screen
            keys "$UP" "$LEFT"                      # and back again
            keys "$A" "$B" "$B" "$B" ;;
        backup-twice)                               # two snapshots in a row
            gpt_menu                                # the submenu reopens on
            keys "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"    # its first row each time
            keys "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            keys "$B" "$B" ;;
        prevent)                                    # GPT item 5
            gpt_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A" "$DOWN" "$A" "$A" "$A"
            confirm
            keys "$A" "$B" "$B" ;;
        bootentries)                                # both read-only screens
            nvram_menu
            keys "$A"                               # View the boot entries
            keys "$DOWN" "$DOWN"                    # scroll it
            keys "$A"                               # dismiss, back to submenu
            keys "$DOWN" "$A"                       # Scan the ESPs
            keys "$DOWN" "$A"                       # scroll, dismiss
            keys "$B" "$B" ;;
        bootnext)                                   # set BootNext, the one-shot
            nvram_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A"   # Boot something once
            keys "$A"                               # the first entry
            keys "$A"                               # review page, continue
            keys "$A"                               # the saved-configuration note
            confirm
            keys "$A"                               # the result
            keys "$B" "$B" ;;
        bootdefault)                                # move an entry to the front
            nvram_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$A"       # Set the default boot entry
            keys "$DOWN" "$A"                       # the second entry, not the first
            keys "$A" "$A"                          # review, then the saved note
            confirm
            keys "$A"
            keys "$B" "$B" ;;
        bootregister)                               # add an entry for a loader
            nvram_menu
            keys "$DOWN" "$DOWN" "$A"               # Register a bootloader
            keys "$A"                               # the first candidate
            keys "$DOWN" "$A"                       # add at the end, not as default
            keys "$A" "$A"                          # review, then the saved note
            confirm
            keys "$A"
            keys "$B" "$B" ;;
        bootrestore)                                # put a saved copy back
            # Wants a boot-NNN.bkp on the ESP, which only a run that changed
            # NVRAM leaves behind: run 'bootregister' first against the same
            # images, with KEEP_VARS=1 here to see it undone.
            nvram_menu
            keys "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$DOWN" "$A"
            keys "$A"                               # the first snapshot
            keys "$A"                               # review page, continue
            keys "$A"                               # the saved-configuration note
            confirm
            keys "$A"
            keys "$B" "$B" ;;
        menu)                                       # walk the menu, choose nothing
            keys "$DOWN" "$DOWN" "$DOWN" "$UP" "$UP" "$B" ;;
        display)                                    # the display screen
            # Opened with View from the main menu, so there is no timer to
            # race any more. RES=none has no framebuffer to turn, and the
            # application offers no such screen: this walk needs a RES.
            keys "$TAB"                             # View, from the menu
            keys "$DOWN" "$DOWN" "$UP"              # text size down, down, up
            keys "$LEFT" "$RIGHT"                   # turn away and back
            keys "$A"                               # done, back to the menu
            keys "$B" ;;                            # exit
        display-mode)                               # take the offered mode
            # The offer is only made where the firmware's own mode is too
            # small to lay the menus out in the largest cell the display
            # could carry, so this walk wants a RES that is: 800x600 is one,
            # and is what a desktop firmware often picks unasked.
            keys "$TAB"                             # View, from the menu
            keys "$TAB"                             # View again: try the mode
            keys "$A"                               # confirm, inside the timer
            keys "$A"                               # done, back to the menu
            keys "$B" ;;                            # exit
        display-revert)                             # say nothing, and get it back
            keys "$TAB"                             # View, from the menu
            keys "$TAB"                             # View again: try the mode
            # Longer than the confirmation is given, so the mode goes back
            # on its own. This is the path a panel that shows nothing takes,
            # and the only one that cannot be checked by pressing anything.
            sleep 10
            keys "$A"                               # done, back to the menu
            keys "$B" ;;                            # exit
        *)
            echo "unknown script: $SCRIPT" >&2; exit 1 ;;
    esac
    sleep 8
}

# One disk instead of two, which is the shape of the machine this tool is
# for and the only way to exercise the picker being skipped. The disk left
# out is test.img, so a run with this set has no post-condition to check.
DISKS=()
if [ "${ONE_DISK:-0}" != 1 ]; then
    DISKS+=(-drive "file=$DIR/test.img,format=raw,if=none,id=testdisk"
            -device nvme,drive=testdisk,serial=TESTDISK)
fi

# A USB stick, for the walks that back up to removable media.
#
# `removable=on` is the whole point and not a detail: it is what sets the
# RMB bit in the SCSI INQUIRY, which is what makes EDK II mark the media
# removable, which is what the application looks at to decide whether it has
# anywhere to offer besides the ESP. Without it QEMU presents a fixed disk
# over USB and the destination menu never appears.
if [ "${USB:-0}" = 1 ]; then
    DISKS+=(-device qemu-xhci,id=xhci
            -drive "file=$DIR/usb.img,format=raw,if=none,id=usbstick"
            -device usb-storage,bus=xhci.0,drive=usbstick,removable=on)
fi

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
    "${DISKS[@]}" \
    -net none \
    -nographic
rc=$?
set -e
echo
# qemu never exits on its own once the keypresses are done, so `timeout`
# kills it on every run and 124 is the normal code. Anything else means the
# machine never ran at all — qemu missing (127), refused the command line,
# or crashed — and without this check every "no-change" and "skip" walk
# would pass green on a host with no qemu installed.
echo "### qemu exited with $rc (script: $SCRIPT) ###"
if [ "$rc" -ne 124 ] && [ "$rc" -ne 0 ]; then
    echo "### FAILED: qemu did not run to the timeout (exit $rc) ###" >&2
    exit 1
fi

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
