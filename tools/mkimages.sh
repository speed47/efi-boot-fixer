#!/usr/bin/env bash
# Build the two images the QEMU harness boots:
#
#   boot.img - GPT disk with an ESP holding the application
#   test.img - SteamOS-shaped disk whose primary GPT we deliberately break
#
# The app must exclude boot.img (it booted from it) and repair test.img.
# No root required: the FAT filesystem is built in its own file and dd'd
# into the partition, rather than going through losetup.
set -euo pipefail
export PATH="$PATH:/usr/sbin:/sbin"

OUT=${1:?usage: mkimages.sh <output-dir> <path-to-efi>}
EFI=${2:?usage: mkimages.sh <output-dir> <path-to-efi>}
CORRUPTION=${3:-zero-header}

mkdir -p "$OUT"
BOOT="$OUT/boot.img"
TEST="$OUT/test.img"
ESP="$OUT/esp.fat"

# ---------------------------------------------------------------- boot disk
rm -f "$BOOT" "$ESP"
truncate -s 96M "$BOOT"
sgdisk -o "$BOOT" >/dev/null
sgdisk -n 1:2048:0 -t 1:ef00 -c 1:ESP "$BOOT" >/dev/null

ESP_START=$(sgdisk -i 1 "$BOOT" | awk '/First sector/ {print $3}')
ESP_END=$(sgdisk -i 1 "$BOOT" | awk '/Last sector/ {print $3}')
ESP_SECTORS=$(( ESP_END - ESP_START + 1 ))

truncate -s $(( ESP_SECTORS * 512 )) "$ESP"
mkfs.vfat -F 32 -n EFIGPTFIX "$ESP" >/dev/null
mmd -i "$ESP" ::/EFI ::/EFI/BOOT
mcopy -i "$ESP" "$EFI" ::/EFI/BOOT/BOOTX64.EFI
dd if="$ESP" of="$BOOT" bs=512 seek="$ESP_START" conv=notrunc status=none
rm -f "$ESP"

# ---------------------------------------------------------------- test disk
rm -f "$TEST"
truncate -s 64G "$TEST"          # sparse
sgdisk -o "$TEST" >/dev/null
sgdisk -n 1:2048:+256M -t 1:ef00 -c 1:esp \
       -n 2:0:+64M    -t 2:8300 -c 2:efi-A \
       -n 3:0:+64M    -t 3:8300 -c 3:efi-B \
       -n 4:0:+5G     -t 4:8300 -c 4:rootfs-A \
       -n 5:0:+5G     -t 5:8300 -c 5:rootfs-B \
       -n 6:0:+256M   -t 6:8300 -c 6:var-A \
       -n 7:0:+256M   -t 7:8300 -c 7:var-B \
       -n 8:0:+10G    -t 8:8300 -c 8:home \
       -n 9:0:+16M    -t 9:0c01 \
       -n 10:0:+20G   -t 10:0700 -c 10:"Basic data partition" "$TEST" >/dev/null

echo "--- test.img table before corruption ---"
sgdisk -p "$TEST" | sed -n '/^Number/,$p'

case "$CORRUPTION" in
  zero-header)  dd if=/dev/zero of="$TEST" bs=512 seek=1 count=1  conv=notrunc status=none ;;
  zero-all)     dd if=/dev/zero of="$TEST" bs=512 seek=1 count=33 conv=notrunc status=none ;;
  bad-crc)      printf '\xff\xff\xff\xff' | dd of="$TEST" bs=1 seek=$((512+16)) conv=notrunc status=none ;;
  # Protective MBR SizeInLBA (offset 446+12). Unlike a broken primary GPT,
  # EDK II does not silently restore this, so it is the corruption that
  # actually reaches the application under OVMF. See the note below.
  bad-mbr)      printf '\x39\x30\x00\x00' | dd of="$TEST" bs=1 seek=458 conv=notrunc status=none ;;
  none)         ;;
  *) echo "unknown corruption mode: $CORRUPTION" >&2; exit 1 ;;
esac

# NOTE: OVMF (EDK II PartitionDxe, PartitionRestoreGptTable) rewrites an
# invalid primary GPT from a valid backup at connect time, before any
# application runs. So under OVMF the 'zero-header' modes are repaired by
# the firmware and our app correctly reports a healthy disk. Use 'bad-mbr'
# to exercise the write path in QEMU; the GPT repair itself is covered by
# the host integration tests, which have no firmware in the way.

echo "--- test.img health after '$CORRUPTION' (sgdisk) ---"
sgdisk -v "$TEST" 2>&1 | grep -E "Main header|Backup header|Main partition|Caution|invalid" || echo "(clean)"
echo "images ready in $OUT"
