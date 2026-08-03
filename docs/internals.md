# Internals

## Layout

```
crates/gptcore/      no_std, no UEFI dependency - parsing, validation, planning
crates/bootfixr/     the EFI_APPLICATION (its own workspace; UEFI target only)
  src/ui/            the menus, and the two backends they can be drawn on
  src/gfx/           framebuffer, rotation, baked font, character console
  src/blockdev.rs    gptcore::BlockDevice over EFI_BLOCK_IO_PROTOCOL
  src/diskinfo.rs    drive vendor/model via EFI_DISK_INFO_PROTOCOL
  src/selfdev.rs     identifying which disk booted this image
  src/nvram.rs       read-only NVRAM boot-option parsing
  src/espscan.rs     read-only scan of every ESP for bootloaders
  src/store.rs       the tool's own snapshots, on the ESP and on any
                     removable volume attached
  src/fwcrc.rs       CRC-32 via the firmware's CalculateCrc32, with a fallback
  src/bin/efiprobe.rs  a second EFI_APPLICATION that reports raw input/display
                     capabilities instead of touching disks; see input.md
tools/               image builders, the QEMU harness, the font rasteriser
```

`gptcore` performs no I/O and knows nothing about firmware. It reads through a
`BlockDevice` trait and takes CRC-32 as an injected dependency, so the exact
logic that decides what to overwrite runs unchanged under `cargo test` against
loopback images. `repair::plan` returns an ordered list of steps rather than
writing anything, which makes the ordering guarantee an assertable data
structure instead of a comment.

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

`crates/gptcore/src/layout.rs` holds the SteamOS A/B partition set, taken from
a real dual-booting Deck rather than from documentation.

That distinction was expensive. The first version guessed the generic "Linux
filesystem" type GUID (`0FC63DAF-…`) for every partition. SteamOS actually
uses the systemd discoverable-partition GUIDs — `4F68BCE3` for `rootfs-A/B`,
`4D21B016` for `var-A/B`, `933AC7E1` for `home` — and `efi-A/B` are Microsoft
basic data. Seven of eight were wrong. Because `recognize()` then required
both name *and* type to match, `rootfs-A` counted as missing, which is a
critical partition, so the verdict came out `RefusedImplausibleSecondary`:
**the tool refused to repair the exact disk it was written for**, and it took
real sectors to find out.

So matching is now by partition **name**, with the type GUID compared and
reported but never fatal. A hardcoded type table is precisely the kind of
thing that goes stale across an OS release, and being strict about it turns a
recovery tool into a brick. A stale or foreign table is still caught, because
its names will not line up either.
