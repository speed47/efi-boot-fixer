#!/usr/bin/env bash
# Boot the application under OVMF with two NVMe disks.
#
# Disk 1 (boot) carries the ESP we boot from and must be excluded by the
# application. Disk 2 (test) is the one with the broken primary GPT.
#
# On a non-x86 host this runs under TCG emulation, so allow a few minutes.
set -euo pipefail

DIR=${1:?usage: run-qemu.sh <image-dir> [confirm]}
CONFIRM=${2:-no}
TIMEOUT=${TIMEOUT:-300}

CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
VARS_SRC=/usr/share/OVMF/OVMF_VARS_4M.fd
VARS="$DIR/vars.fd"
cp -f "$VARS_SRC" "$VARS"

# OVMF's boot manager needs a nudge to reach the fallback loader promptly,
# and the app itself waits for a typed confirmation.
# Under TCG the boot takes a variable 40-90s, so the confirmation is sent
# more than once. Keys typed before the prompt sit in the firmware's input
# buffer and are consumed when the app starts reading, and a surplus
# "REPAIR" just falls through to the final "press Enter".
if [ "$CONFIRM" = "yes" ]; then
    INPUT_CMD='sleep 60; printf "REPAIR\r"; sleep 25; printf "REPAIR\r"; sleep 25; printf "\r"; sleep 10'
else
    INPUT_CMD='sleep 100; printf "\r"; sleep 10'
fi

set +e
bash -c "$INPUT_CMD" | timeout "$TIMEOUT" qemu-system-x86_64 \
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
echo "### qemu exited with $rc ###"
