# efigptfix

A UEFI application for inspecting, repairing, backing up and restoring GUID
partition tables, targeting a Steam Deck that dual-boots Windows and SteamOS.

It runs from the ESP under the firmware's "boot from file" menu, which works
even with the primary table destroyed because the Deck firmware falls back to
the backup GPT for partition enumeration. The Linux kernel does not fall back
without the `gpt` cmdline option, which is why `steamcl.efi` loads and then
dies at `pivot_root`.

Five operations, all driven with the D-pad:

| Operation | Writes | What it does |
| --- | --- | --- |
| Check GPT | never | reads both tables and reports every defect |
| Repair primary GPT | disk | rebuilds a corrupt primary from the backup table |
| Back up both GPTs | ESP file | snapshots both tables to `\EFIGPTFIX\` on the ESP |
| Restore GPTs | disk | writes a saved snapshot back |
| Prevent recurrence | disk | closes the `FirstUsableLBA` gap (see below) |

Everything that touches a disk is gated behind a five-press confirmation
sequence and shows exactly which LBAs it will write first.

## Layout

```
crates/gptcore/      no_std, no UEFI dependency - parsing, validation, planning
crates/efigptfix/    the EFI_APPLICATION (its own workspace; UEFI target only)
tools/               image builders and the QEMU harness
```

`gptcore` performs no I/O and knows nothing about firmware. It reads through a
`BlockDevice` trait and takes CRC-32 as an injected dependency, so the exact
logic that decides what to overwrite runs unchanged under `cargo test` against
loopback images. `repair::plan` returns an ordered list of steps rather than
writing anything, which makes the ordering guarantee an assertable data
structure instead of a comment.

## Building

Requires a Rust toolchain with the `x86_64-unknown-uefi` target:

```sh
rustup target add x86_64-unknown-uefi
make build          # -> crates/efigptfix/target/x86_64-unknown-uefi/release/efigptfix.efi
make               # list every target
```

`make dist` stages the binary plus a `SHA256SUMS` in `build/dist`, and
`make install ESP=/boot/efi` copies it onto a mounted ESP without touching
NVRAM. `make check` runs exactly what CI runs.

Note that the UEFI application is a separate workspace from `gptcore`, so it
needs its own cargo invocation with an explicit `--target`; the Makefile
handles that. It also prefers `~/.cargo/bin/cargo` when present, because
distro cargo packages generally ship no std for the UEFI target.

## Testing

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

Under QEMU + OVMF, with two NVMe disks: `boot.img` carries the ESP the image
is launched from and appears as disk 1, `test.img` is a SteamOS-shaped disk
and appears as disk 2.

```sh
make images CORRUPTION=bad-mbr
./tools/run-qemu.sh build/images repair   # or check, backup, restore, prevent, menu
sgdisk -v build/images/test.img           # verify the result independently
```

`run-qemu.sh` drives the menus over the serial console, where OVMF's
`TerminalDxe` turns `ESC[A`..`ESC[D` into D-pad scan codes, CR into A and a
lone ESC into B — the same alphabet the Deck's buttons produce, so these runs
exercise the real input path rather than a keyboard-only one. `repair-boot`
targets disk 1, which is how the write-to-your-own-boot-disk case gets
tested.

Corruption modes: `zero-header`, `zero-all`, `bad-crc`, `bad-mbr`, `hybrid`,
`none`. `hybrid` adds a second MBR partition record beside the `0xEE` one,
which OVMF leaves alone, so it is the way to reach the hybrid-MBR refusal
under firmware.

`mkimages.sh` also rewrites `FirstUsableLBA` to 2048 in both headers,
resealing the CRCs, so the QEMU disk has the same shape as the Deck's:
sgdisk writes 34, and the gap is the whole subject of the prevention
operation.

### OVMF repairs the primary GPT before your application runs

Worth knowing before you spend an afternoon on it: EDK II's `PartitionDxe`
calls `PartitionRestoreGptTable()` at connect time, which **rewrites an
invalid primary GPT from a valid backup** before any application is loaded.
So under OVMF the `zero-header` and `zero-all` modes produce a disk that is
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

## CI

`.github/workflows/ci.yml` does four things:

| Job | Trigger | What it does |
| --- | --- | --- |
| `test` | every push and PR | `fmt-check`, `clippy -D warnings`, full test suite (installs `gdisk`) |
| `build` | every push and PR | builds the `.efi`, asserts it really is a PE32+ x86_64 EFI application, uploads it as an artifact |
| `continuous` | push to the default branch | deletes and recreates the `continuous` **prerelease** with the binary attached |
| `release` | tag `v*` | creates a **draft** release with the binary attached |

The `continuous` release is deliberately deleted and recreated rather than
edited, so its tag always points at the current `main`. Both the release and
its tag are removed with `|| true`, since the first run has neither and a
cancelled run can leave a tag without a release.

The QEMU harness is not in CI: driving the menus over a serial console
depends on boot timing, which is exactly the kind of thing that turns into a
flaky job. Run `make qemu SCRIPT=repair` locally instead.

## Input: the Deck has no keyboard

Everything about the interface follows from one constraint: there is no
built-in keyboard, Bluetooth is not available in the firmware environment,
and few people have a USB-C keyboard to hand. The buttons, sticks, trackpads
and touchscreen are the only realistic inputs, and how the firmware exposes
them is not something to guess at.

`efiprobe.efi` answers that empirically. It enumerates the input protocols
the firmware publishes and logs every event it sees:

| Protocol | What it would give us |
| --- | --- |
| `EFI_SIMPLE_TEXT_INPUT_PROTOCOL` | scan codes and Unicode chars, i.e. buttons mapped to keys |
| `EFI_SIMPLE_POINTER_PROTOCOL` | relative motion from trackpads or a mouse |
| `EFI_ABSOLUTE_POINTER_PROTOCOL` | the touchscreen, with its coordinate range |

Everything goes to `efiprobe.log` **on the ESP it was launched from**,
flushed after every line, as well as to the screen. The screen scrolls and
cannot be copied off the device; the file can be read from Linux afterwards,
and cutting the power keeps whatever was logged up to that moment. The probe
walks 30 guided steps at 6 seconds each plus a 20-second free-form phase,
then exits on its own after roughly 200 seconds, because without a keyboard
there may be no way to tell it to stop.

```sh
make probe-esp ESP=/path/to/esp     # installs EFI/efiprobe.efi
# boot menu -> boot from file -> efiprobe.efi
# press each control in turn, then read EFI/../efiprobe.log
```

Verified end to end under OVMF, including recovering the log file from the
ESP afterwards.

### What a Steam Deck actually reports

Measured on real hardware, firmware `Valve rev 0x10033`, UEFI 2.70. The raw
capture is in `docs/efiprobe-deck.log`.

| Control | Event |
| --- | --- |
| A | unicode `0x000D` (CR) |
| QAM (three dots) | unicode `0x000D` (CR) — **indistinguishable from A** |
| B | scan `0x17` (ESCAPE) |
| Menu / burger | scan `0x17` (ESCAPE) — **indistinguishable from B** |
| D-pad up / down / left / right | scan `0x01` / `0x02` / `0x04` / `0x03` |
| View (two rectangles) | unicode `0x0009` (TAB) |
| L2 trigger | `SimplePointer[1]` **right** button |
| R2 trigger | `SimplePointer[1]` **left** button |
| Right trackpad | `SimplePointer[1]` relative dx/dy, click = left button |
| Left trackpad | `SimplePointer[1]` dz only, i.e. a scroll wheel |
| X, Y | nothing |
| L1, R1 bumpers | nothing |
| L4, L5, R4, R5 back buttons | nothing |
| Both sticks: click and movement | nothing |
| STEAM button | nothing |
| Touchscreen: tap and drag | nothing |

So the usable set is: **CR**, **ESCAPE**, **four D-pad scan codes**, **TAB**,
and a relative pointer with two buttons and a scroll axis.

Three results worth calling out:

**Keys auto-repeat while held.** Holding A for six seconds produced 63 CR
events, about 10.5/s. That makes hold-to-confirm possible, which matters:
`EFI_SIMPLE_TEXT_INPUT_PROTOCOL` reports presses with no key-release event,
so without auto-repeat there would be no way to detect a held button at all.
D-pad DOWN also repeats but far slower, around 1.8/s.

**Input is buffered.** The step after the "hold A" test recorded 7 stray CR
events before its own. A confirmation gate therefore cannot simply count
events; it has to require them to keep *arriving*, and reset on a gap.

**The touchscreen is not available.** An `EFI_ABSOLUTE_POINTER_PROTOCOL` is
published with an 0..65536 range, but neither a tap nor a drag produced a
single event. No touch-target interface is possible.

## Deploying to the Deck

Copy to the ESP and invoke it manually from the firmware menu. Deliberately
**not** an NVRAM boot entry, since SteamOS rewrites the boot order on update:

```sh
cp efigptfix.efi /esp/EFI/efigptfix.efi
```

Then boot menu -> "Boot from file" -> the ESP -> `efigptfix.efi`. Secure Boot
must be off, as the binary is unsigned.

### Using it

A full-screen D-pad menu with a highlight bar, because the hardware has no
keyboard. `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL` provides cursor positioning and
sixteen colours, so no extra dependencies are involved. The selected item's
description appears below the list, which doubles as inline help:

```
efigptfix
------------------------------------------------------------------
  version 0.1.0
  launched from PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,...)/HD(1,GPT,...)

  Check GPT                                          <- highlighted
   Repair primary GPT from the backup
   Back up both GPTs to the ESP
   Restore GPTs from a saved backup
   Prevent recurrence (close the FirstUsableLBA gap)
   Exit

  Read both tables and report what is wrong.
  Writes nothing.
  D-pad = move    A = choose    B = exit
```

Choosing an operation opens a disk picker, which is where the identifying
detail lives:

```
Repair primary GPT
------------------------------------------------------------------
  Choose a disk. Removable media is not listed.
  [boot] carries the volume this program came from.

   Disk 1    96.0 MiB  [boot]
  Disk 2   931.5 GiB  [SteamOS]                      <- highlighted

  PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,...)
  1953525168 blocks x 512 B
  GPT: primary GPT is repairable from the backup
```

`[SteamOS]` is inferred from the table itself: an ESP plus either two Linux
root partitions, two `/var` partitions, or two names forming an `-A`/`-B`
pair. That structure is what makes a SteamOS install recognisable at a
glance and it survives any single partition being renamed. A drive model is
shown when the firmware will give one — see "Identifying drives" below.

Reports paginate with the D-pad, since a Deck cannot scroll back. Writes
are authorised by a fixed sequence rather than a held button:

```
  To authorise this, press in order:

     LEFT  RIGHT  LEFT  RIGHT  A
     [x]   [x]    [ ]   [ ]    [ ]

  next: LEFT
  B = cancel, nothing is written
```

Colour carries meaning rather than decoration. Each line is tagged by the
code that writes it — `gptcore::style::Style` — with the mapping to actual
colours living in the application:

| Style | Colour | Used for |
| --- | --- | --- |
| `Title` | white | captions introducing a block |
| `Normal` | light grey | body text |
| `Dim` | dark grey | device paths, column headings, provenance |
| `Good` | light green | healthy, verified, succeeded |
| `Warn` | yellow | needs attention, caveats worth reading |
| `Bad` | light red | damage, refusal, failure |
| `Key` | light cyan | the value you must actually take in: an LBA about to be overwritten, the disk you are pointed at |

The UEFI side never decides a colour by looking at the text, so rewording a
message cannot silently change what it looks like. In the confirmation gate
each progress box is coloured individually — completed steps green, the one
being waited for cyan, the rest dim.

Any wrong press resets it; B cancels outright. A sequence was chosen over
hold-to-confirm because it depends only on discrete presses: it does not
rely on the firmware's auto-repeat, and a buffered burst of repeats cannot
walk through it. Every screen that asks a question drains queued input
first, for the same reason.

## What it will and will not touch

Applied in order, before anything is written:

- whole disks only (`Media->LogicalPartition == FALSE`)
- never removable media, never read-only media — so the SD card and USB
  sticks never appear as repair targets
- never a disk with a hybrid MBR (some legacy OS depends on that view)
- never a backup table that fails structural checks (overlaps, ranges outside
  the usable area, inverted extents) or whose entry array would collide with
  the first usable LBA
- never a table without an `esp` and a `rootfs-A`, which is the stale-backup
  guard
- never a saved snapshot whose block size or disk size does not match
- never without the operator entering the confirmation sequence

### Not excluding the boot disk

Earlier versions refused to write to the disk this image booted from, and
refused to write anywhere at all if that disk could not be identified. That
was backwards. The whole point is to live on the Deck's own ESP so a broken
partition table can be fixed without a USB stick or a keyboard — which makes
the boot disk the disk that needs repairing. It is now labelled `[boot]` in
the picker and otherwise treated like any other.

One real consequence: `OpenProtocolAttributes::Exclusive` on a whole disk
disconnects the partition and filesystem drivers serving it, including the
one serving the ESP this program was loaded from. The running image survives
(it is already in memory), but ESP access afterwards may not, so file
operations happen before block writes and a warning appears if you go back to
the backup or restore screens afterwards. If the firmware refuses the
exclusive open entirely, the write falls back to a shared open rather than
failing — being unable to repair the machine's only drive would be the worse
outcome — and the result screen says which happened. This path is exercised
under OVMF by `run-qemu.sh build/images repair-boot`.

The repair rewrites `MyLBA`, `AlternateLBA` and `PartitionEntryLBA` field by
field rather than copying the backup block, and recomputes both CRCs. The
entry array is written and flushed *before* the header that points at it, so
a power cut cannot leave a valid header describing garbage.

`gBS->CalculateCrc32` is used when available, so the checksums written are
produced by the same code that validates them at boot; `gptcore`'s own
implementation is the fallback, and the app says which one it used.

Windows partitions alongside SteamOS are expected and are not treated as
suspicious.

## Backup and restore

Snapshots go to `\EFIGPTFIX\gpt.001`, `gpt.002`, ... on the ESP the program
was launched from. A sequence number rather than a timestamp for two
reasons: it fits 8.3, so the name reads the same from firmware, Windows and
Linux; and it does not depend on the clock, which firmware may decline to
give. Numbering counts up from the highest present and never fills a gap —
reusing the number of a deleted snapshot would make the ordering lie about
which is newest. The date lives inside the file, where the picker shows it.

The file is not a `dd` of the first 34 sectors. Each structure is stored as a
separate chunk with a role — protective MBR, primary entry array, primary
header, backup entry array, backup header — alongside the geometry it came
from, the disk GUID, and the health of the table at the time. That buys three
things a raw dump does not:

- restore refuses a disk whose block size or block count differs, instead of
  writing a table that describes a different device;
- the operator is told, before authorising, whether the snapshot was taken
  from a healthy table — restoring a corrupt one is a real way to make things
  worse, and the screen says so in as many words;
- the write order puts entry arrays on the medium and flushes them before the
  headers that name them, exactly as a repair does.

Everything is little-endian and ends with a CRC32 over the whole file, so a
truncated or bit-rotted snapshot is rejected outright rather than
half-restored. Files that fail to decode are listed as rejected and never
offered as a choice.

The checksum is written by `gBS->CalculateCrc32` under firmware and verified
by `gptcore`'s own implementation on the host; a snapshot taken under OVMF
was confirmed byte-for-byte against `zlib.crc32`, so archives are portable
between the two.

A caveat the tool states on screen: when the ESP is on the disk being backed
up, this is a convenience copy, not an off-device backup.

### Choosing between snapshots years later

The restore screen lists every snapshot with its date, partition count,
capacity and the health of the table when it was taken, and works out which
of the attached disks each one belongs to:

```
  gpt.001  2026-08-01 17:37:54  10 parts   64.0 GiB  healthy     <- highlighted
   gpt.002  2026-08-01 17:38:15  10 parts   64.0 GiB  healthy
   gpt.003  2026-08-01 17:43:11   1 parts   96.0 MiB  healthy

  Belongs to: Disk 2 - 10 of 10 partitions still carry the same unique GUID
  disk GUID 0FBB6478-4344-4767-A49E-A95B8F30CCF8
  written by efigptfix 0.1.0
  D-pad = move    A = choose    View = details    B = back
```

Attribution leans on the **per-partition unique GUIDs**, not the disk GUID.
Those are generated once when a partition is created and survive OS
upgrades, so a snapshot sharing most of them with the disk in front of you
is that disk's, whatever else has changed — whereas the disk GUID is a
single field any partitioner may rewrite. Geometry alone would be a weak
answer on a machine with two identical drives.

**View** opens the full record, which is what the format version 2 metadata
section exists for:

```
  Taken:        2026-08-01 17:38:15
  State then:   healthy
  Belongs to:   Disk 2 - 10 of 10 partitions still carry the same unique GUID

  Identity
    Disk GUID     0FBB6478-4344-4767-A49E-A95B8F30CCF8
    Geometry      134217728 blocks x 512 B = 64.0 GiB
    Usable range  2048..134217694
    Entry array   128 entries x 128 B at LBA 2

  Recorded when it was written
    tool          efigptfix 0.1.0
    firmware      Valve rev 0x10033
    uefi          2.70
    device        PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,...)
    capacity      1953525168 blocks x 512 B
    launched-from PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,...)/HD(1,GPT,...)

  Partitions (11)
     #     Start LBA       Size  Name                 Unique GUID
     1          2048  256.0 MiB  esp                  D8ED3710-AA9C-...
```

Provenance is key/value text rather than struct fields, deliberately: this
is read by a person years later, and an unknown key they can still read
beats a decoder that refuses the file. `tools/deck-corrupt.py` writes the
same section, and on Linux it can record the drive model and serial from
sysfs — which UEFI will not give for NVMe. `deck-corrupt.py show <file>`
prints all of it without root or a device.

**Version 1 snapshots stay readable.** A backup is worthless if a later
build refuses it, so `decode` accepts both layouts and only `encode` moved
on; there is a test that downgrades a snapshot to version 1 and restores
from it.

## Identifying drives

There is no protocol that hands over "vendor and model".
`EFI_DISK_INFO_PROTOCOL` exposes the raw identification payload of whichever
transport the drive is on, and what that contains differs per transport:

| Transport | Call | Payload |
| --- | --- | --- |
| SCSI, USB mass storage | `Inquiry()` | SCSI INQUIRY — vendor at byte 8, product at 16 |
| ATA, AHCI | `Identify()` | ATA IDENTIFY DEVICE — model at byte 54, byte-swapped per word |
| NVMe | `Identify()` | *namespace* data, which carries no model string; `Inquiry()` returns `EFI_NOT_FOUND` |

So on the Deck's internal NVMe drive there is nothing to show, and the picker
falls back to capacity, flags and device path. The extraction is best-effort
throughout and returns nothing rather than guessing: a wrong drive name in a
list of disks you are about to overwrite is worse than no name at all. Any
payload that does not decode to printable ASCII is discarded.

## The expected layout, and why matching is by name

`crates/gptcore/src/layout.rs` holds the SteamOS A/B partition set, taken
from a real dual-booting Deck rather than from documentation.

That distinction was expensive. The first version guessed the generic "Linux
filesystem" type GUID (`0FC63DAF-…`) for every partition. SteamOS actually
uses the systemd discoverable-partition GUIDs — `4F68BCE3` for `rootfs-A/B`,
`4D21B016` for `var-A/B`, `933AC7E1` for `home` — and `efi-A/B` are
Microsoft basic data. Seven of eight were wrong. Because `recognize()` then
required both name *and* type to match, `rootfs-A` counted as missing, which
is a critical partition, so the verdict came out `RefusedImplausibleBackup`:
**the tool refused to repair the exact disk it was written for**, and it took
real sectors to find out.

So matching is now by partition **name**, with the type GUID compared and
reported but never fatal. A hardcoded type table is precisely the kind of
thing that goes stale across an OS release, and being strict about it turns a
recovery tool into a brick. A stale or foreign backup is still caught,
because its names will not line up either.

## What the corruption actually is

Captured from the affected Deck, the damage is two bytes in LBA 1:

```
PartitionEntryLBA:  2  ->  2016
HeaderCRC32:               recomputed so the header still verifies
```

Nothing else differs. The protective MBR, the entire entry array at LBA
2-33, and the backup GPT are all untouched — `gptbackup` and
`gptbackupfixed` are byte-identical, so gdisk's repair only rewrote the
primary header.

Two things follow, and they shaped the code:

**The header CRC is valid.** Whatever wrote this recomputed it correctly.
A GPT checker that validates only the header CRC — which is the obvious
thing to write — reports this disk as perfectly healthy. What catches it is
the partition-entry-array CRC (the array is read from 2016, finds nothing,
and fails to match) and an explicit check that a primary header points at
LBA 2. `tests/deck_corrupt.rs` asserts both, and asserts the *absence* of a
header CRC defect so nobody later "simplifies" the check away.

**2016 is 2048 - 32.** The entry array is 32 sectors, and `FirstUsableLBA`
on this disk is 2048. So the writer computed
`PartitionEntryLBA = FirstUsableLBA - entry_array_sectors`, i.e. it placed
the array immediately below the first usable block instead of at LBA 2.

On a conventionally-formatted disk `FirstUsableLBA` is 34, and that formula
gives `34 - 32 = 2`, which is correct. It only goes wrong when there is a
gap between the entry array and the first usable block — and this table was
written by util-linux fdisk, which leaves exactly such a gap (sgdisk warns
about it). That is a plausible reason the corruption recurs here and not on
most machines, and it suggests a permanent fix: setting `FirstUsableLBA` to
34 would make the buggy arithmetic produce the right answer. No partition
moves, since the first one starts at 2048 either way.

That is the "prevent recurrence" operation. It rewrites only the two header
blocks, and refuses unless both tables are healthy, the primary array is at
LBA 2, the headers agree on `FirstUsableLBA`, and no partition starts below
the proposed value. It is presented on screen as a **theory that fits the
observed numbers exactly, not a diagnosis**, with its own separate
confirmation, because unlike a repair it modifies a healthy table on a
hypothesis. It is reversible: set the value back. (The user confirmed the
corruption followed a Windows 24H2 → 25H2 upgrade, which is reportedly
well-known behaviour; that is consistent with the arithmetic but does not
prove the mechanism.)

## Reproducing the corruption on purpose

`tools/deck-corrupt.py` inflicts exactly the damage that was found in the
wild — `PartitionEntryLBA` moved from 2 to `FirstUsableLBA - array_blocks`,
header CRC recomputed so the header still verifies, six bytes, nothing else
— so the repair path can be tested against the real failure rather than a
synthetic one.

```sh
sudo ./tools/deck-corrupt.py inspect /dev/nvme0n1                       # never writes
sudo ./tools/deck-corrupt.py break   /dev/nvme0n1 -o /esp/EFIGPTFIX/gpt-before.bin
sudo ./tools/deck-corrupt.py restore /dev/nvme0n1 -i /esp/EFIGPTFIX/gpt-before.bin
```

`break` refuses unless it has first written a snapshot and read it back
through its own parser, and unless both tables *and* the protective MBR
verify going in. The important one is the backup GPT: corrupting the primary
when the backup could not repair it is the one outcome the script must never
produce, and there is a test for that refusal.

The snapshot is written in efigptfix's own archive format, so it can be put
back three ways: `restore` here, the EFI application's "Restore GPTs from a
saved backup", or by hand from the sector dump inside it. Regular files are
accepted as well as block devices, so the whole thing can be rehearsed
against a disk image first.

`tests/corrupt_script.rs` runs the script against the real Deck fixture and
asserts that the changed bytes are exactly `512+16..19` and `512+72..73`,
that gptcore's verdict is `PrimaryRepairable` with a `PrimaryEntryLbaNotTwo`
defect and *no* header CRC defect, that the snapshot decodes with
`backup::decode`, and that both recovery routes restore the first 34 sectors
byte for byte.

A snapshot written by the script has been read off an ESP and restored by
the EFI application under OVMF. Note what that run does and does not show:
it proves the archive is portable between the two, but not that the restore
repaired the corruption, because OVMF had already rebuilt the primary from
the backup before the application was loaded. The byte-exact recovery is
proven by the host tests, where no firmware sits in the way.

## Testing against real hardware

`crates/gptcore/tests/data/deck/` holds real sectors from a dual-booting
Deck: `head.bin` (LBA 0-33), `tail.bin` (last 33 LBAs) and the sector count.
The disk GUID and every unique partition GUID are replaced with obvious
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

To capture a dump from your own machine, see the dd recipe in
`tools/reconstruct.py`'s docstring.
