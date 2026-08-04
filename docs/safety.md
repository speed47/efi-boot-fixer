# What it will and will not touch

Applied in order, before anything is written:

- whole disks only (`Media->LogicalPartition == FALSE`)
- never removable media, never read-only media — so the SD card and USB
  sticks never appear as repair targets. This is a rule about *disks*, and
  the backup screens are not covered by it: a snapshot is a file, written
  through a filesystem, and removable media is offered there on purpose —
  see [backups.md](backups.md). Nothing writes blocks to a removable disk.
- never a disk with a hybrid MBR (some legacy OS depends on that view)
- never a secondary GPT that fails structural checks (overlaps, ranges outside
  the usable area, inverted extents) or whose entry array would collide with
  the first usable LBA
- never a table without an `esp` and a `rootfs-A`, which is the stale-table
  guard
- never a saved snapshot whose block size or disk size does not match
- never without the operator entering the confirmation sequence, on a screen
  whose header names the disk about to be written to — which is what allows
  the picker to be skipped when there is only one disk to pick

Prevent recurrence, being experimental, adds its own refusals on top: the
table must already be healthy, `PartitionEntryLBA` must already be 2 (the
shape this operation exists to close, not to repair), both headers must
agree on where the entry array lives, geometry must be known, and no
partition may already sit where the closed gap would need to start.

The NVRAM screens add their own, and touch no disk at all:

- only ESPs on fixed disks are scanned, so a loader on a USB stick never
  becomes a boot entry that breaks when the stick is pulled
- the whole boot configuration is saved before the first change of a
  session — to the ESP, to removable media, or to both — and a failure to
  save it anywhere is a question rather than a silent skip; see
  [boot.md](boot.md)
- a new entry is written before the `BootOrder` that names it, never after
- no boot entry is ever deleted; removing one is left to the firmware's own
  menu, and restoring a saved configuration overwrites the variables that
  snapshot holds and no others
- the same confirmation sequence applies, including to `BootNext`, which
  reverts itself and could have been exempted

"Boot a loader now (chainloading)" touches neither NVRAM nor a disk — it loads and starts a
candidate the scan found directly, in memory, for this session only — so it
gets a single acknowledgement rather than the confirmation sequence, the same
as Reboot and Shutdown. What it hands control to is not vetted beyond what
`espscan` already reports: an unrecognised `.efi` is offered like any other,
because refusing to start it would just be the type-GUID mistake pointed at a
different list. See [boot.md](boot.md#booting-a-loader-immediately).

## Not excluding the boot disk

Earlier versions refused to write to the disk this image booted from, and
refused to write anywhere at all if that disk could not be identified. That
was backwards. The whole point is to live on the Deck's own ESP so a broken
partition table can be fixed without a USB stick or a keyboard — which makes
the boot disk the disk that needs repairing. It is now labelled `[boot]` in
the picker and otherwise treated like any other.

One real consequence: `OpenProtocolAttributes::Exclusive` on a whole disk
disconnects the partition and filesystem drivers serving it, including the one
serving the ESP this program was loaded from. The running image survives (it
is already in memory), but ESP access afterwards may not, so file operations
happen before block writes and a warning appears if you go back to the backup
or restore screens afterwards. If the firmware refuses the exclusive open
entirely, the write falls back to a shared open rather than failing — being
unable to repair the machine's only drive would be the worse outcome — and the
result screen says which happened. This path is exercised under OVMF by
`run-qemu.sh build/images repair-boot`.

## Write ordering and checksums

The repair rewrites `MyLBA`, `AlternateLBA` and `PartitionEntryLBA` field by
field rather than copying the secondary GPT's block, and recomputes both CRCs. The
entry array is written and flushed *before* the header that points at it, so a
power cut cannot leave a valid header describing garbage.

`gBS->CalculateCrc32` is used when available, so the checksums written are
produced by the same code that validates them at boot; `gptcore`'s own
implementation is the fallback, and the app says which one it used.

Windows partitions alongside SteamOS are expected and are not treated as
suspicious.
