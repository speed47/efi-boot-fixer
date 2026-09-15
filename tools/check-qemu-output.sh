#!/usr/bin/env bash
# Kept separate so a failed walk can be checked again without another boot.
set -euo pipefail

RAW=${1:?usage: check-qemu-output.sh <serial-log> <screen-log> <script> <corruption>}
TEXT=${2:?missing screen-log}
SCRIPT=${3:?missing script}
CORRUPTION=${4-}

# Keep every repaint, not just the final screen. Colours can divide a word;
# cursor positioning divides rows. Removing both indiscriminately would
# glue the footer to the next title and hide useful line boundaries.
python3 - "$RAW" "$TEXT" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-?]*[ -/]*([@-~])",
              lambda m: "" if m[1] in "mhl" else "\n", text)
text = re.sub(r"\x1b[^\[]", "", text)
pathlib.Path(sys.argv[2]).write_text(text)
PY

fail() {
    echo "### FAILED: '$SCRIPT' screen output: $* ###" >&2
    echo "Inspect $TEXT (raw serial: $RAW). Check BOOT_WAIT/STEP if keys missed a screen." >&2
    exit 1
}

require() {
    grep -aFq -- "$1" "$TEXT" || fail "missing '$1'"
}

require_re() {
    grep -aEq -- "$1" "$TEXT" || fail "missing pattern '$1'"
}

reject() {
    if grep -aFq -- "$1" "$TEXT"; then fail "unexpected '$1'"; fi
}

saved() {
    require 'Saved as:'
    require_re "\\\\BOOTFIXR\\\\$1-[0-9]+\\.$2"
}

gpt_health() {
    require_re 'Main GPT[[:space:]]*: OK'
    require_re 'Secondary GPT[[:space:]]*: OK'
}

# Menu labels alone prove only that the application started. Each operation
# also needs its own report body, result, or explicit refusal below.
require 'Check this machine'
require 'Exit to the firmware'
reject ' FAILED'
reject 'But not everywhere:'
reject 'Could not save a snapshot'
reject 'Could not save the boot configuration'
reject 'This copy is PARTIAL.'

case "$SCRIPT" in
    none) ;;
    menu)
        require '> Boot entries (NVRAM) ...'
        require '> Generate a diagnostic report [read only]' ;;
    overview)
        require 'Check this machine [read only]'
        require 'GPT: healthy, nothing to do'
        require 'in the boot order'
        require 'not referenced by any NVRAM boot entry'
        require 'Nothing was written. This screen never modifies anything.' ;;
    check|check-one)
        require 'Check GPT (read only)'
        gpt_health
        require 'Current table:'
        require 'Nothing was written. This check never modifies a disk.'
        if [ "$SCRIPT" = check-one ]; then
            reject 'Choose a disk.'
            require 'healthy, nothing to do'
        else
            require 'Choose a disk.'
            require 'rootfs-A'
            require 'rootfs-B'
        fi ;;
    repair|repair-cancel|repair-boot)
        gpt_health
        if [ "$SCRIPT" = repair-boot ]; then
            require 'healthy, nothing to do'
            reject 'Authorise write'
        elif [ "$CORRUPTION" = bad-mbr ]; then
            require 'only the protective MBR needs rewriting'
            require 'The table as it stands was first saved to:'
            require 'This rewrites the protective MBR on this disk.'
            require 'Authorise write'
            if [ "$SCRIPT" = repair-cancel ]; then
                require 'Cancelled. Nothing was written.'
                reject 'Written and flushed.'
            else
                require 'Written and flushed.'
                require 'The disk now reports: healthy, nothing to do'
            fi
        else
            case "$CORRUPTION" in
                hybrid) require 'hybrid MBR present - refusing to touch this disk' ;;
                none|zero-header|zero-all|bad-crc) require 'healthy, nothing to do' ;;
                *) fail "no repair expectation for corruption '$CORRUPTION'" ;;
            esac
            reject 'Authorise write'
            reject 'Written and flushed.'
        fi ;;
    prevent)
        if [ "$CORRUPTION" = hybrid ]; then
            require 'this disk carries a hybrid MBR'
            reject 'Authorise write'
            reject 'Written and flushed.'
        else
            require 'FirstUsableLBA would move from 2048 to 34.'
            require 'Prevent recurrence: what will be written'
            require 'Authorise write'
            require 'Written and flushed.'
            require 'The disk now reports:'
        fi ;;
    backup|backup-twice|backup-usb|backup-usb-only)
        saved gpt bkp
        if [ "$SCRIPT" = backup-twice ]; then
            # Count distinct saved paths, not repaints of the same result.
            count=$(grep -aoE '\\BOOTFIXR\\gpt-[0-9]+\.bkp' "$TEXT" | sort -u | wc -l)
            [ "$count" -ge 2 ] || fail 'did not save two different snapshots'
        fi ;;
    restore|restore-usb|restore-source-error)
        require "snapshot(s) in \\BOOTFIXR\\"
        require 'Restore onto which disk?'
        require 'The table as it stands was first saved to:'
        require 'This REPLACES both partition tables with the saved copy.'
        require 'Written and flushed.'
        require 'The disk now reports:'
        if [ "$SCRIPT" = restore-usb ]; then require 'found on RESCUE'; fi
        if [ "$SCRIPT" = restore-source-error ]; then
            require 'Some files could not be read and are not offered'
        fi ;;
    inspect|scroll)
        require_re 'Snapshot gpt-[0-9]+\.bkp'
        require 'State then:'
        require 'Belongs to:'
        require 'Identity'
        if [ "$SCRIPT" = inspect ]; then
            count=$(grep -aoE 'Snapshot gpt-[0-9]+\.bkp' "$TEXT" | sort -u | wc -l)
            [ "$count" -ge 2 ] || fail 'did not inspect two different snapshots'
        else
            require 'Recorded when it was written'
            require 'Partitions (10)'
        fi ;;
    report|report-usb)
        require 'Diagnostic report (read only)'
        require 'generated by'
        saved diag txt
        require 'Copy it off this machine and attach it whole.' ;;
    bootentries)
        require 'Boot entries in NVRAM (read only)'
        require 'Nothing was written. This screen never modifies NVRAM.'
        require 'Bootloaders on the ESPs (read only)'
        require '\EFI\steamos\steamcl.efi'
        require '\EFI\Microsoft\Boot\bootmgfw.efi'
        require '\EFI\weird\mystery.efi' ;;
    bootnext|bootdefault|bootregister|bootrestore)
        require 'Boot configuration saved'
        require 'Authorise NVRAM write'
        require 'Written to NVRAM.'
        case "$SCRIPT" in
            bootnext) require 'This sets the next boot only. It reverts itself.' ;;
            bootdefault) require 'Only the order is changed; no entry is rewritten.' ;;
            bootregister) require "This adds a boot entry to the firmware's NVRAM." ;;
            bootrestore)
                require 'Restore boot configuration from backup'
                require 'This OVERWRITES the boot entries and the boot order' ;;
        esac ;;
    bootbackup) saved boot bkp ;;
    *) fail "no screen assertions defined for '$SCRIPT'" ;;
esac

case "$SCRIPT" in
    backup-usb|backup-usb-only|report-usb)
        require 'on RESCUE'
        if [ "$SCRIPT" = backup-usb-only ]; then
            reject 'on the volume this program was launched from'
        else
            require 'on the volume this program was launched from'
        fi ;;
esac

case "$SCRIPT" in
    none|menu|overview|check|check-one|bootentries|inspect|scroll|repair-boot|repair-cancel)
        reject 'Written and flushed.'
        reject 'Written to NVRAM.' ;;
esac

echo "### screen output verified for '$SCRIPT' ($TEXT) ###"
