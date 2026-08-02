# Testing

Host unit and integration tests. The integration tests build real disk images
with `sgdisk`, corrupt them, repair them, and then ask `sgdisk` whether it is
satisfied — an independent implementation is the oracle, so a bug shared
between our reader and our writer cannot hide:

```sh
cargo test              # needs gdisk installed
```

Note that `sgdisk -v` prints "No problems found" on a disk with a wrecked
primary, because it transparently falls back to the backup and reports on the
table it loaded. The harness therefore also requires the absence of any
`Caution`/`Warning`/`ERROR` text before calling a disk healthy.

## Under QEMU and OVMF

With two NVMe disks: `boot.img` carries the ESP the image is launched from and
appears as disk 1, `test.img` is a SteamOS-shaped disk and appears as disk 2.

```sh
make images CORRUPTION=bad-mbr
./tools/run-qemu.sh build/images repair   # or overview, check, backup, restore, prevent, menu
sgdisk -v build/images/test.img           # verify the result independently
```

`run-qemu.sh` drives the menus over the serial console, where OVMF's
`TerminalDxe` turns `ESC[A`..`ESC[D` into D-pad scan codes, CR into A and a
lone ESC into B — the same alphabet the Deck's buttons produce, so these runs
exercise the real input path rather than a keyboard-only one. `repair-boot`
targets disk 1, which is how the write-to-your-own-boot-disk case gets tested.

The graphical backend writes nothing to the serial console, so a run that
exercises it has to be photographed rather than read:

```sh
make qemu-shots                          # 800x1280 framebuffer, i.e. rotated
make qemu-shots QRES=1280x800            # landscape, i.e. not rotated
SCRIPT=display make qemu-shots           # View, then the display screen
```

Screendumps land in `build/shots` as PPM, one every few seconds, taken over
QMP by `tools/qemu-shots.py`. `RES` is delivered as the EDID preferred mode of
the emulated VGA adapter, which is the way to get OVMF outside its built-in
mode table — its video PCDs are fixed at build time and ignore `fw_cfg`.

`RES=none` removes the video adapter altogether, so the firmware publishes no
graphics protocol and the application falls back to the text console. That run
prints to serial like any other, which is the point of testing it:

```sh
RES=none ./tools/run-qemu.sh build/images menu
```

Corruption modes: `zero-header`, `zero-all`, `bad-crc`, `bad-mbr`, `hybrid`,
`none`. `hybrid` adds a second MBR partition record beside the `0xEE` one,
which OVMF leaves alone, so it is the way to reach the hybrid-MBR refusal
under firmware.

`mkimages.sh` also rewrites `FirstUsableLBA` to 2048 in both headers,
resealing the CRCs, so the QEMU disk has the same shape as the Deck's: sgdisk
writes 34, and the gap is the whole subject of the prevention operation.

### OVMF repairs the primary GPT before your application runs

Worth knowing before you spend an afternoon on it: EDK II's `PartitionDxe`
calls `PartitionRestoreGptTable()` at connect time, which **rewrites an
invalid primary GPT from a valid backup** before any application is loaded. So
under OVMF the `zero-header` and `zero-all` modes produce a disk that is
already fixed by the time this tool runs, and it will correctly report
`Primary GPT: OK`. That is the firmware working, not a false negative.

Use `bad-mbr` to exercise the write path in QEMU, since the protective MBR is
not restored that way. The GPT repair itself is covered by the host
integration tests, where no firmware sits between the code and the image.

This also implies something about the Deck: its firmware evidently does *not*
do this restore, or the primary would come back healthy on its own and this
tool would be unnecessary. If you ever run this on the Deck and it reports a
healthy primary immediately after a failed boot, that assumption is what
changed.

## Testing against real hardware

`crates/gptcore/tests/data/deck/` holds real sectors from a dual-booting Deck:
`head.bin` (LBA 0-33), `tail.bin` (last 33 LBAs) and the sector count. The
disk GUID and every unique partition GUID are replaced with obvious
placeholders and the CRCs resealed, so nothing identifies the physical drive;
type GUIDs, names and extents are untouched.

`tests/deck_fixture.rs` rebuilds a full-size sparse image from those dumps at
runtime — a 931.5 GiB image that costs 512 bytes on disk — and runs the real
analysis and repair against it. `tools/reconstruct.py` does the same thing
from the command line, with `--scrub` to produce a shareable fixture:

```sh
tools/reconstruct.py /path/to/dump /tmp/deck.img          # faithful
tools/reconstruct.py /path/to/dump /tmp/deck.img --scrub  # de-identified
```

To capture a dump from your own machine — three files, none of which contain
any file data, only the partition tables:

```sh
DEV=/dev/nvme0n1
mkdir dump && cd dump
sudo blockdev --getsz $DEV > sectors.txt                       # 512-byte sectors
sudo dd if=$DEV of=head.bin bs=512 count=34                    # LBA 0..33
sudo dd if=$DEV of=tail.bin bs=512 count=33 \
        skip=$(( $(cat sectors.txt) - 33 ))                    # the last 33 LBAs
```

Then run `tools/reconstruct.py dump /tmp/deck.img --scrub` before sharing the
result: the raw dump still carries the disk GUID and the unique GUID of every
partition on the machine it came from.

The corruption itself can be reproduced on a real disk or an image with
`tools/deck-corrupt.py` — see [corruption.md](corruption.md).
