# efigptfix

A UEFI application that rebuilds a corrupt primary GPT from the backup GPT,
targeting a Steam Deck that dual-boots Windows and SteamOS.

It runs from the ESP under the firmware's "boot from file" menu, which works
even with the primary table destroyed because the Deck firmware falls back to
the backup GPT for partition enumeration. The Linux kernel does not fall back
without the `gpt` cmdline option, which is why `steamcl.efi` loads and then
dies at `pivot_root`.

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

Under QEMU + OVMF, with two NVMe disks so the boot-device exclusion is
genuinely exercised (the app boots from one and must repair the other):

```sh
./tools/mkimages.sh /tmp/imgs crates/efigptfix/target/x86_64-unknown-uefi/release/efigptfix.efi bad-mbr
./tools/run-qemu.sh /tmp/imgs yes     # 'yes' types the confirmation
sgdisk -v /tmp/imgs/test.img          # verify the result independently
```

Corruption modes: `zero-header`, `zero-all`, `bad-crc`, `bad-mbr`, `none`.

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

The QEMU harness is not in CI: driving the confirmation prompt over a serial
console depends on boot timing, which is exactly the kind of thing that turns
into a flaky job. Run `make qemu-confirm` locally instead.

## Deploying to the Deck

Copy to the ESP and invoke it manually from the firmware menu. Deliberately
**not** an NVRAM boot entry, since SteamOS rewrites the boot order on update:

```sh
cp efigptfix.efi /esp/EFI/efigptfix.efi
```

Then boot menu -> "Boot from file" -> the ESP -> `efigptfix.efi`. Secure Boot
must be off, as the binary is unsigned.

## What it will and will not touch

Applied in order, before anything is written:

- whole disks only (`Media->LogicalPartition == FALSE`)
- never removable media, never read-only media
- never the device it booted from, identified by comparing the candidate
  disk's device path against the boot volume's as a prefix — so the SD card
  and USB sticks are excluded, and so is the disk hosting its own ESP
- **nothing at all** if the boot device cannot be identified
- never a disk with a hybrid MBR (some legacy OS depends on that view)
- never a backup table that fails structural checks (overlaps, ranges outside
  the usable area, inverted extents) or whose entry array would collide with
  the first usable LBA
- never a table without an `esp` and a `rootfs-A`, which is the stale-backup
  guard
- never without the operator typing `REPAIR` at the prompt

The repair rewrites `MyLBA`, `AlternateLBA` and `PartitionEntryLBA` field by
field rather than copying the backup block, and recomputes both CRCs. The
entry array is written and flushed *before* the header that points at it, so
a power cut cannot leave a valid header describing garbage.

`gBS->CalculateCrc32` is used when available, so the checksums written are
produced by the same code that validates them at boot; `gptcore`'s own
implementation is the fallback, and the app says which one it used.

Windows partitions alongside SteamOS are expected and are not treated as
suspicious.

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
most machines, and it suggests a possible permanent fix: setting
`FirstUsableLBA` to 34 would make the buggy arithmetic produce the right
answer. No partition moves, since the first one starts at 2048 either way.
gdisk can do it from the experts' menu with `j`. Untested — a hypothesis
that fits the arithmetic exactly, not a diagnosis.

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
