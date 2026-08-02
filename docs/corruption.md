# What the corruption actually is

Captured from the affected Deck, the damage is two bytes in LBA 1:

```
PartitionEntryLBA:  2  ->  2016
HeaderCRC32:               recomputed so the header still verifies
```

Nothing else differs. The protective MBR, the entire entry array at LBA 2-33,
and the backup GPT are all untouched — `gptbackup` and `gptbackupfixed` are
byte-identical, so gdisk's repair only rewrote the primary header.

Two things follow, and they shaped the code:

**The header CRC is valid.** Whatever wrote this recomputed it correctly. A
GPT checker that validates only the header CRC — which is the obvious thing to
write — reports this disk as perfectly healthy. What catches it is the
partition-entry-array CRC (the array is read from 2016, finds nothing, and
fails to match) and an explicit check that a primary header points at LBA 2.
`tests/deck_corrupt.rs` asserts both, and asserts the *absence* of a header
CRC defect so nobody later "simplifies" the check away.

**2016 is 2048 - 32.** The entry array is 32 sectors, and `FirstUsableLBA` on
this disk is 2048. So the writer computed
`PartitionEntryLBA = FirstUsableLBA - entry_array_sectors`, i.e. it placed the
array immediately below the first usable block instead of at LBA 2.

On a conventionally-formatted disk `FirstUsableLBA` is 34, and that formula
gives `34 - 32 = 2`, which is correct. It only goes wrong when there is a gap
between the entry array and the first usable block — and this table was
written by util-linux fdisk, which leaves exactly such a gap (sgdisk warns
about it). That is a plausible reason the corruption recurs here and not on
most machines, and it suggests a permanent fix: setting `FirstUsableLBA` to 34
would make the buggy arithmetic produce the right answer. No partition moves,
since the first one starts at 2048 either way.

## Prevent recurrence

That is what the "prevent recurrence" operation does. It rewrites only the two
header blocks, and refuses unless both tables are healthy, the primary array
is at LBA 2, the headers agree on `FirstUsableLBA`, and no partition starts
below the proposed value. It is presented on screen as a **theory that fits
the observed numbers exactly, not a diagnosis**, with its own separate
confirmation, because unlike a repair it modifies a healthy table on a
hypothesis. It is reversible: set the value back.

(The user confirmed the corruption followed a Windows 24H2 → 25H2 upgrade,
which is reportedly well-known behaviour; that is consistent with the
arithmetic but does not prove the mechanism.)

## Reproducing the corruption on purpose

`tools/deck-corrupt.py` inflicts exactly the damage that was found in the wild
— `PartitionEntryLBA` moved from 2 to `FirstUsableLBA - array_blocks`, header
CRC recomputed so the header still verifies, six bytes, nothing else — so the
repair path can be tested against the real failure rather than a synthetic
one.

```sh
sudo ./tools/deck-corrupt.py inspect /dev/nvme0n1                       # never writes
sudo ./tools/deck-corrupt.py break   /dev/nvme0n1 -o /esp/BOOTFIXR/gpt-before.bin
sudo ./tools/deck-corrupt.py restore /dev/nvme0n1 -i /esp/BOOTFIXR/gpt-before.bin
```

`break` refuses unless it has first written a snapshot and read it back
through its own parser, and unless both tables *and* the protective MBR verify
going in. The important one is the backup GPT: corrupting the primary when the
backup could not repair it is the one outcome the script must never produce,
and there is a test for that refusal.

The snapshot is written in bootfixr's own archive format, so it can be put
back three ways: `restore` here, the EFI application's "Restore GPTs from a
saved backup", or by hand from the sector dump inside it. Regular files are
accepted as well as block devices, so the whole thing can be rehearsed against
a disk image first.

`tests/corrupt_script.rs` runs the script against the real Deck fixture and
asserts that the changed bytes are exactly `512+16..19` and `512+72..73`, that
gptcore's verdict is `PrimaryRepairable` with a `PrimaryEntryLbaNotTwo` defect
and *no* header CRC defect, that the snapshot decodes with `backup::decode`,
and that both recovery routes restore the first 34 sectors byte for byte.

A snapshot written by the script has been read off an ESP and restored by the
EFI application under OVMF. Note what that run does and does not show: it
proves the archive is portable between the two, but not that the restore
repaired the corruption, because OVMF had already rebuilt the primary from the
backup before the application was loaded (see [testing.md](testing.md)). The
byte-exact recovery is proven by the host tests, where no firmware sits in the
way.
