# Backup and restore

Snapshots go to `\BOOTFIXR\gpt.001`, `gpt.002`, ... on the ESP the program was
launched from. A sequence number rather than a timestamp for two reasons: it
fits 8.3, so the name reads the same from firmware, Windows and Linux; and it
does not depend on the clock, which firmware may decline to give. Numbering
counts up from the highest present and never fills a gap — reusing the number
of a deleted snapshot would make the ordering lie about which is newest. The
date lives inside the file, where the picker shows it.

> Upgrading from a build older than the rename: earlier versions kept their
> snapshots in `\EFIGPTFIX`. Move them across or Restore will not offer them:
> `mv /esp/EFIGPTFIX /esp/BOOTFIXR`.

## The file format

The file is not a `dd` of the first 34 sectors. Each structure is stored as a
separate chunk with a role — protective MBR, main entry array, main header,
secondary entry array, secondary header — alongside the geometry it came
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
by `gptcore`'s own implementation on the host; a snapshot taken under OVMF was
confirmed byte-for-byte against `zlib.crc32`, so archives are portable between
the two.

A caveat the tool states on screen: when the ESP is on the disk being backed
up, this is a convenience copy, not an off-device backup.

## Choosing between snapshots years later

The restore screen lists every snapshot with its date, partition count,
capacity and the health of the table when it was taken, and works out which of
the attached disks each one belongs to:

```
  gpt.001  2026-08-01 17:37:54  10 parts   64.0 GiB  healthy     <- highlighted
   gpt.002  2026-08-01 17:38:15  10 parts   64.0 GiB  healthy
   gpt.003  2026-08-01 17:43:11   1 parts   96.0 MiB  healthy

  Belongs to: Disk 2 - 10 of 10 partitions still carry the same unique GUID
  disk GUID 0FBB6478-4344-4767-A49E-A95B8F30CCF8
  written by bootfixr 0.1.0
  D-pad = move    A = choose    View = details    B = back
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
