# The diagnostic report

`Generate a diagnostic report` writes everything this machine will say into one
plain text file: `\BOOTFIXR\diag-001.txt`, `diag-002.txt`, ... It reads the
disks, the variable store, the ESPs and the volumes, and writes nothing but
that file.

It exists for a reader who is not in the room. Every other screen in this
tool is built for somebody standing in front of the machine, and is
therefore ruthless about what it leaves out — a screen that says everything
says nothing on a 7-inch panel. But the person who actually solves a strange
boot failure is usually whoever reads the forum post afterwards, and they
cannot press View, cannot ask a cheap follow-up question, and have no way to
know which of the forty things the machine could have said were the ones
that got dropped. So the report drops none of them.

## What is in it

| Section | What it answers |
| --- | --- |
| This machine (SMBIOS) | Manufacturer, product, version, SKU, family and a masked serial and UUID; the BIOS vendor, version and release date; the baseboard, chassis and processor |
| Firmware and this program | Vendor, revision, UEFI revision, where this image was launched from, which CRC32 was used, the screen it drew on, how much NVRAM is left, and which configuration tables the firmware publishes |
| Secure Boot | `SecureBoot`, `SetupMode`, `AuditMode`, `DeployedMode`, `VendorKeys`, and the size of `PK`, `KEK`, `db`, `dbx`, `dbt` |
| Block devices | Every handle carrying `EFI_BLOCK_IO_PROTOCOL`, including the ones the repair paths refuse, and why each is refused |
| Partition tables | Per disk: the verdict, the four MBR records as numbers, every field of both GPT headers, and every partition's range, type, unique GUID and attributes |
| Boot entries in NVRAM | `BootCurrent`, `BootNext`, `Timeout`, `BootOrder`, then every `Boot####` in the store with its position, flags, device path and load options in hex |
| Bootloaders on the ESPs | Each ESP, its partition GUID, every `.efi` found on it and whether NVRAM points at it |
| Volumes | Where files can be saved, how much room is left, and what is already in `\BOOTFIXR` |
| Every variable in the store | The name, size and vendor GUID of every variable the firmware will enumerate |

The last one is a stock-take rather than a diagnosis, and it is last for that
reason. It has earned its place by being useful exactly when the rest of the
report looks fine: a variable store with two hundred stale entries in it, or
a `Boot0003` sitting under a vendor GUID nobody expected, is invisible
everywhere else in this tool.

There are no passwords in it and no disk contents — only tables, variables
and the *names* of files on the ESPs.

## Serial numbers

Every serial number SMBIOS carries — system, baseboard, chassis, processor,
plus the asset tags and the system UUID — is **masked**, keeping the length
and the first three characters:

```
    serial            : FMT********* (masked)
    UUID              : 4C4C4544-****-****-****-************
```

Enough survives to match two reports of the same machine to each other, and
to see that the firmware still holds the field; not enough for a stranger
reading the thread to quote the number back to a warranty desk. These are
the only identifiers in the file with a fixed offset, a known meaning, and a
use to somebody impersonating the owner, which is why they are the ones
singled out.

The one exception is `Unknown`, firmware's own placeholder for a serial or
asset tag nobody ever programmed: it is left as `Unknown` rather than
`Unk**** (masked)`, since masking it would make an empty field look like a
real one.

**Masking a serial does not make the file anonymous**, and nothing in the
tool says it does. Three things still identify the machine and cannot be
masked without gutting the report:

- **Partition unique GUIDs**, which are how `backup::compare` decides a
  snapshot belongs to a disk, and which are in every `gpt-NNN.bkp` already.
- **Device paths**: a `UsbWwid()` node *is* a device serial number by
  definition, and an `NVMe()` node carries the namespace EUI-64.
- **Boot entry descriptions**, which firmware often builds out of the device
  string — `UEFI SanDisk Cruzer 4C530001234567890` — with no fixed offset to
  key on. A boot entry's load options, dumped as hex, are also where some
  loaders keep a kernel command line.

That distinction is on the screen that reports the file was written, and in
the file's own preamble, not only here. The person deciding whether to post
it has read neither this document nor the source.

## Two deliberate differences from the screens

**Nothing is wrapped.** Screens wrap long values because a Deck cannot
scroll sideways. A device path is the single most useful line in a report
about a machine that will not boot, and one broken across three lines cannot
be searched for, diffed against another machine's, or pasted back into a
command. The 78-column rules are only rules.

**The file is CRLF.** It is written to a FAT volume that gets carried to
whichever machine the operator has to hand. Everything that reads LF also
reads CRLF; the reverse is not true of every editor that ships on Windows,
and failing to open is failure at exactly the step this feature exists for.

The lines still carry a `Style`, because the report is shown on screen before
it is saved — the same page, in colour, with the findings picked out. An
operator who only wanted to read it has what they came for without writing a
file at all.

## Where it goes

Through the same destination menu as a snapshot, for the same reason, and
with the same rules — see [backups.md](backups.md). A copy on the ESP of a
machine that will not boot is a copy nobody can fetch, so a USB stick is
offered whenever one is plugged in, and one report saved to two places gets
**one** name, numbered from what is in use on every volume the tool can see.

Names count up from the highest present and never fill a gap. A report is
cheaper to lose than a partition table, but two different reports sharing a
name is the same failure, and the person most likely to be holding both is
the one comparing "before" and "after".

## Reading one

```
--- Partition tables ---------------------------------------------------------

  Disk 2    64.0 GiB  [SteamOS]
  PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,00-00-00-00-00-00-00-00)
  134217728 blocks x 512 B = 64.0 GiB
  Protective MBR: wrong size (12345 blocks, expected 134217727)
  Main GPT      : OK
  Secondary GPT : OK
  => only the protective MBR needs rewriting

  MBR partition records at LBA 0:
     #  boot  type     start LBA        blocks  start CHS  end CHS
     1  0x00  0xEE             1         12345  00 02 00   FF FF FF
     2  (unused)
     3  (unused)
     4  (unused)

  Main GPT header:
    signature         : 0x5452415020494645  "EFI PART"
    ...
    entry array LBA   : 2
```

The four MBR records are printed whether or not they are healthy, because
"protective" is a judgement and these are the evidence for it: the difference
between a protective MBR and the hybrid one this tool refuses to touch is
four rows of numbers.

The secondary GPT's partition list is compared against the main one rather
than printed twice. On a healthy disk they are identical and a second copy is
forty lines nobody reads; when they differ, that difference is the most
important thing in the report and is stated rather than left to be spotted.

## Where the code is

Split the same way [report.rs](../crates/gptcore/src/report.rs) is, and for
the same reason:

- `gptcore::diag` — the shape of the file, the naming, and everything that
  can be rendered from a disk image alone. Tested on the host against the
  real Deck sectors in `crates/gptcore/tests/diag_report.rs`.
- `bootfixr::diag` — the half that needs firmware: the variable store, the
  protocols, the volumes.
- `gptcore::smbios` — the SMBIOS structure walk and its string sets, again
  byte-level and host-tested. The table is copied out of firmware memory
  before anything reads it, by the ten lines in `bootfixr::smbios`, so that
  a garbled length is a short section rather than a machine that stops.
- `bootfixr::secureboot` — the five flags and five databases, read-only.
  Enrolling or clearing a platform key is a decision with consequences this
  tool has no way to explain on a screen with no keyboard, so it is not
  offered; the state is worth reporting because it explains a refusal.
