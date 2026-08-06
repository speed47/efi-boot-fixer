# Testing

Host unit and integration tests, in `crates/gptcore/tests/`:

- `repair_images.rs` builds real disk images with `sgdisk`, corrupts them,
  repairs them, and then asks `sgdisk` whether it is satisfied — an
  independent implementation is the oracle, so a bug shared between our
  reader and our writer cannot hide
- `backup_restore.rs` covers the snapshot format, including decoding an
  older format version
- `bootopt.rs` covers NVRAM load-option parsing, `bootwrite.rs` the write
  plans described in [boot.md](boot.md)
- `corrupt_script.rs` and `deck_corrupt.rs` exercise `tools/deck-corrupt.py`
  and its refusal conditions, alongside [corruption.md](corruption.md)
- `deck_fixture.rs` is covered in "Testing against real hardware" below

```sh
make test               # everything below, needs gdisk installed
make test-unit          # cargo test --lib: gptcore's unit tests only, no gdisk needed
make test-integration   # cargo test --test repair_images: the sgdisk-backed images
```

Note that `sgdisk -v` prints "No problems found" on a disk with a wrecked
main GPT, because it transparently falls back to the secondary and reports on
the table it loaded. The harness therefore also requires the absence of any
`Caution`/`Warning`/`ERROR` text before calling a disk healthy.

## Under QEMU and OVMF

With two NVMe disks: `boot.img` carries the ESP the image is launched from and
appears as disk 1, `test.img` is a SteamOS-shaped disk and appears as disk 2.

```sh
make images CORRUPTION=bad-mbr
./tools/run-qemu.sh build/images repair   # or overview, check, backup, restore, prevent, menu
sgdisk -v build/images/test.img           # verify the result independently
```

`make qemu-repair` is a shortcut for the two `make images` + `run-qemu.sh
... repair` steps above. `make qemu SCRIPT=<name>` covers the rest of the
scripts, including the boot-entry ones (`bootnext`, `bootdefault`,
`bootregister`, `bootbackup`, `bootrestore`) exercised in [boot.md](boot.md).

`make qemu-check` runs every walk `run-qemu.sh` knows, one after another,
each against its own freshly built images so an earlier walk's leftovers
never fool a later one's post-condition check -- except the pairs that are
*supposed* to share a build (`backup-usb-only`+`restore-usb`,
`backup-twice`+`inspect`+`scroll`, `bootregister`+`bootrestore`), which
`tools/qemu-test-all.sh` knows about and runs back to back on purpose. It
exits non-zero and lists which walks failed if any of them did not verify.
Expect it to take a long time -- it is dozens of QEMU boots, each with its
own `BOOT_WAIT` -- and to need OVMF, `sgdisk` and `mtools` installed, same
as the individual walks above.

`run-qemu.sh` drives the menus over the serial console, where OVMF's
`TerminalDxe` turns `ESC[A`..`ESC[D` into D-pad scan codes, CR into A and a
lone ESC into B — the same alphabet the Deck's buttons produce, so these runs
exercise the real input path rather than a keyboard-only one. `repair-boot`
targets disk 1, which is how the write-to-your-own-boot-disk case gets tested.

`USB=1` attaches `usb.img` as a removable USB stick, which is the only way to
reach the destination menu: with nothing removable present the tool does not
ask where a backup should go, because there is only one answer. The stick is
a bare labelled FAT volume — no partition table, no ESP type GUID — because
that is what the tool is supposed to accept.

```sh
make images
USB=1 ./tools/run-qemu.sh build/images backup-usb        # to both places
mdir -i build/images/usb.img ::/BOOTFIXR                 # the copy on the stick
mdir -i build/images/boot.img@@1048576 ::/BOOTFIXR       # and the one on the ESP
```

Both files carry the same name and are byte-for-byte identical; `cmp` on the
two extracted copies is the assertion. `backup-usb-only` writes to the stick
alone, and `restore-usb` puts the copy from it back:

```sh
make images                                              # start from nothing
USB=1 ./tools/run-qemu.sh build/images backup-usb-only   # the only snapshot,
USB=1 ./tools/run-qemu.sh build/images restore-usb       # on the stick
```

`backup-usb-only` specifically, and on fresh images: `restore-usb` presses A on
the first row, so the run only proves anything if the single snapshot offered
can have come from nowhere but the stick. After a `backup-usb` there would be
a copy on the ESP too, and — since the launch volume is listed first — that is
the row it would land on, testing the path that already had a walk.

The `report` and `report-usb` walks save a diagnostic report, and the file
they leave behind is the whole assertion — it is the one output of this tool
that can be read directly rather than inferred from a disk digest:

```sh
make images
USB=1 ./tools/run-qemu.sh build/images report-usb         # to both places
mcopy -n -i build/images/usb.img ::/BOOTFIXR/diag-001.txt /dev/stdout
```

Worth reading in full after any change to what it gathers: a firmware that
answers a protocol differently shows up there as a missing section rather
than as a build failure. `report` without `USB=1` writes to the ESP alone
and skips the destination menu, which is the shape of the run on hardware
with nothing plugged in. See [report.md](report.md).

`ONE_DISK=1` leaves `test.img` off the machine, which is the only way to
exercise the picker being skipped — the shape of the hardware this tool is
actually for. The `check-one` walk presses once where two presses would
otherwise be needed, so what it proves is in the serial log rather than on a
disk:

```sh
RES=none ONE_DISK=1 ./tools/run-qemu.sh build/images check-one | tee one.log
grep -c "Choose a disk" one.log        # 0: the picker never appeared
grep -o "Check GPT (read only)" one.log # the report, reached in one press
```

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

### OVMF repairs the main GPT before your application runs

Worth knowing before you spend an afternoon on it: EDK II's `PartitionDxe`
calls `PartitionRestoreGptTable()` at connect time, which **rewrites an
invalid main GPT from a valid secondary** before any application is loaded. So
under OVMF the `zero-header` and `zero-all` modes produce a disk that is
already fixed by the time this tool runs, and it will correctly report
`Main GPT: OK`. That is the firmware working, not a false negative.

Use `bad-mbr` to exercise the write path in QEMU, since the protective MBR is
not restored that way. The GPT repair itself is covered by the host
integration tests, where no firmware sits between the code and the image.

This also implies something about the Deck: its firmware evidently does *not*
do this restore, or the main GPT would come back healthy on its own and this
tool would be unnecessary. If you ever run this on the Deck and it reports a
healthy main GPT immediately after a failed boot, that assumption is what
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
