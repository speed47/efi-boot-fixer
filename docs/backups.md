# Backup and restore

Snapshots go to `\BOOTFIXR\gpt-001.bkp`, `gpt-002.bkp`, ... A sequence number
rather than a timestamp for two reasons: it fits 8.3, so the name reads the
same from firmware, Windows and Linux; and it does not depend on the clock,
which firmware may decline to give. Numbering counts up from the highest
present and never fills a gap — reusing the number of a deleted snapshot
would make the ordering lie about which is newest. The date lives inside the
file, where the picker shows it.

## Where they go

The ESP the program was launched from is the default, and used to be the only
choice. It is the one volume guaranteed to be there in the situation this tool
exists for — no keyboard, no stick, the machine will not boot — so a copy
there is a copy the operator can definitely reach.

It is also, very often, a copy on the disk being backed up. That is the whole
weakness of it, and it is why a USB stick or an SD card is offered as a second
destination whenever one is plugged in:

```
  Removable media is attached, so a copy can be kept
  somewhere other than this machine's own disk.

  Save to both                            <- highlighted
   Save to the ESP only
   Save to RESCUE only

  One copy on the ESP, one on RESCUE.
  The ESP copy is the one this program can always find
  again; the other one survives the disk.
```

With nothing attached the question is not asked at all — every answer would be
the same one — and the screen that reports what was written says that plugging
something in first would have offered more. Volumes are taken as they come:
**any** removable, writable, present filesystem qualifies, partitioned or not,
ESP-typed or not, because a stick formatted by a phone is still somewhere a
40 KiB file can live. The only removable volume deliberately left out is the
one the program was launched from, which running off a rescue stick makes
normal: it is already offered, as the launch volume.

Saving to both writes **the same name to both places** — one snapshot, one
number, wherever it ends up. The number is chosen from what is already in use
on *every* volume the tool can see, not only on the ones being written to. Ask
less than that and one name comes to mean two things: save to the stick now
and to the ESP later, and each gets a `gpt-001.bkp` holding a different table,
which is precisely the pair the restore screen cannot help you tell apart.

A destination whose directory cannot be listed is **dropped, and the others
are still written**. It cannot be written to itself — a number picked from an
incomplete listing can collide with a snapshot nobody can see — but making
that fatal would be worse than it sounds: on the NVRAM path a misbehaving
stick would otherwise talk the operator into changing boot variables with no
saved copy at all, on a machine whose ESP was writable the whole time. Writing
then goes ahead on every remaining destination even after one has failed, and
the result screen names both outcomes: a full stick must not hide the copy
that did land on the ESP.

Restore is the mirror of it and asks nothing: it lists what it finds in
`\BOOTFIXR` on the ESP *and* on whatever removable media is attached at the
time, with the volume each snapshot came from on its detail line. The two
copies of one snapshot carry the same name, so that line is what tells them
apart. A volume that will not open is reported as a rejection rather than
emptying the screen — an unreadable stick must not stand between the operator
and the snapshots sitting on the ESP.

The same choice, and the same rules, apply to the `boot-NNN.bkp` snapshot of
the NVRAM boot configuration; see [boot.md](boot.md).

## The file format

The file is not a `dd` of the first 34 sectors. Each structure is stored as a
separate chunk with a role — protective MBR, main entry array, main header,
secondary entry array, secondary header — alongside the geometry it came
from, the disk GUID, and the health of the table at the time. That buys three
things a raw dump does not:

- restore refuses a disk whose block size or block count differs, instead of
  writing a table that describes a different device;
- restore refuses a disk that now carries a hybrid MBR — the same refusal
  repair and prevention make, for the same reason — and refuses any snapshot
  whose entry-array chunks would land inside the area its own table hands to
  partitions, which is what a snapshot taken while a header pointed somewhere
  wild would otherwise write back there;
- the operator is told, before authorising, whether the snapshot was taken
  from a healthy table — restoring a corrupt one is a real way to make things
  worse, and the screen says so in as many words;
- the write order puts entry arrays on the medium and flushes them before the
  headers that name them, exactly as a repair does.

Everything is little-endian and ends with a CRC32 over the whole file, so a
truncated or bit-rotted snapshot is rejected outright rather than
half-restored. Files that fail to decode are listed as rejected and never
offered as a choice.

Snapshots are not only taken by hand. A repair saves one to the ESP by
itself, between the review page and the confirmation gate, so "back up
before you repair" is enforced by the operation rather than by menu
ordering. The metadata section records why each file exists under the
`label` key — "manual backup", "automatic, before repair" — and the restore
picker shows it, which is what lets an operator with six rows of `gpt-NNN`
recall which is which. It is a metadata key rather than a format bump on
purpose: older builds skip keys they do not know, and files they wrote
simply have no label.

The listing now sweeps removable media the operator keeps their own files on,
so anything over 4 MiB is rejected on sight rather than read: a coincidental
namesake of a few hundred MiB would otherwise be pulled into memory in full
merely to be turned down, and an allocation the firmware refuses is not a
rejection line — in `no_std` it is the allocation error handler.

The checksum is written by `gBS->CalculateCrc32` under firmware and verified
by `gptcore`'s own implementation on the host; a snapshot taken under OVMF was
confirmed byte-for-byte against `zlib.crc32`, so archives are portable between
the two.

A caveat the tool states on screen: when the ESP is on the disk being backed
up, that copy is a convenience, not an off-device backup. Choosing a removable
destination as well replaces the caveat with the reason it existed — the
removable copy is the one that survives losing the disk.

## Choosing between snapshots years later

The restore screen lists every snapshot with its date, partition count,
capacity and the health of the table when it was taken, and works out which of
the attached disks each one belongs to:

```
  gpt-001.bkp  2026-08-01 17:37:54  10 parts   64.0 GiB  healthy     <- highlighted
   gpt-002.bkp  2026-08-01 17:38:15  10 parts   64.0 GiB  healthy
   gpt-003.bkp  2026-08-01 17:43:11   1 parts   96.0 MiB  healthy

  Belongs to: Disk 2 - 10 of 10 partitions still carry the same unique GUID
  disk GUID 0FBB6478-4344-4767-A49E-A95B8F30CCF8
  written by bootfixr 0.1.0
  ----------------------------------------------------------
  [D-pad] move   [A] choose   [View] details   [B] back
```

Attribution leans on the **per-partition unique GUIDs**, not the disk GUID.
Those are generated once when a partition is created and survive OS upgrades,
so a snapshot sharing most of them with the disk in front of you is that
disk's, whatever else has changed — whereas the disk GUID is a single field
any partitioner may rewrite. Geometry alone would be a weak answer on a
machine with two identical drives.

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
    tool          bootfixr 0.1.0
    firmware      Valve rev 0x10033
    uefi          2.70
    device        PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,...)
    capacity      1953525168 blocks x 512 B
    launched-from PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,...)/HD(1,GPT,...)

  Partitions (11)
     #     Start LBA       Size  Name                 Unique GUID
     1          2048  256.0 MiB  esp                  D8ED3710-AA9C-...
```

Provenance is key/value text rather than struct fields, deliberately: this is
read by a person years later, and an unknown key they can still read beats a
decoder that refuses the file. `tools/deck-corrupt.py` writes the same
section, and on Linux it can record the drive model and serial from sysfs —
which UEFI will not give for NVMe. `deck-corrupt.py show <file>` prints all of
it without root or a device.

**Version 1 snapshots stay readable.** A snapshot is worthless if a later build
refuses it, so `decode` accepts both layouts and only `encode` moved on; there
is a test that downgrades a snapshot to version 1 and restores from it.
