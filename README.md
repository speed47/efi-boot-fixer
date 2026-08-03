# EFI Boot Fixer for Steam Deck

A UEFI application for inspecting, repairing, backing up and restoring GUID
partition tables (GPT), along with viewing and modifying the NVRAM boot entries
that might get modified or deleted during a SteamOS or Windows upgrade in a
dual-boot setup.

## The reason it was created

When dual-booting SteamOS and Windows, more often than not you'll run into issues
when upgrading them, as they don't tolerate each other very well, and like to take
all the room for themselves (well, especially Windows).

The upgrade to Windows 24H2 is especially infamous with this, symptoms are either:

- Black screen when booting SteamOS, even manually through `steamcl.efi`
- Dropping you to a `grub>` prompt after attempting to boot
- Verbose boot logs ending in `ERROR: Mounting /dev/disk/bypartuuid/<UUIDOFYOURPARTITION> failed.`

This precise upgrade is known to corrupt the main GPT, this is what this tool
was written for, but it now supports more features to (hopefully) always be able to salvage
your Steam Deck that no longer boots, even if you're on the go and don't have any
USB keyboard at hand, and no USB recovery key.

## Features

All driven with the D-pad, from one read-only summary and two submenus:

| Operation | Writes | What it does |
| --- | --- | --- |
| **Check this machine** | never | every disk's table, the boot list and the loaders on the ESPs, on one page |
| *Partition tables (GPT)* | | |
| Check a disk's GPT | never | reads both tables and reports every defect |
| Back up both GPTs | ESP file | snapshots both tables to `\BOOTFIXR\` on the ESP |
| Restore GPTs | disk | writes a saved snapshot back |
| Repair main GPT | disk | rebuilds a corrupt main GPT from the secondary GPT |
| Prevent recurrence | disk | closes the `FirstUsableLBA` gap that causes the damage |
| *Boot entries (NVRAM)* | | |
| View the boot entries | never | the firmware's boot list, including entries that have fallen out of it |
| Scan the ESPs | never | the loaders installed on disk, and whether NVRAM points at each |
| Register a bootloader | NVRAM | adds a boot entry for a loader nothing points at |
| Set the default | NVRAM | moves an entry to the front of the boot order |
| Boot something once | NVRAM | tries an entry without committing to it |
| Restore the boot configuration | NVRAM | writes back a saved copy of the entries and the order |

- **No keyboard needed.** The Deck's buttons are the only input; the menus,
  reports and prompts are built for a D-pad and two buttons.
  See [docs/input.md](docs/input.md).
- **Nothing is written by accident.** Every operation that touches a disk
  shows exactly which LBAs it will overwrite and then requires a five-press
  confirmation sequence. See [docs/using.md](docs/using.md).
- **It refuses more than it accepts.** No removable or read-only media, no
  hybrid MBRs, no implausible secondary GPTs, no mismatched snapshots.
  See [docs/safety.md](docs/safety.md).
- **Snapshots you can still read years later.** Structured, checksummed,
  self-describing archives on the ESP, attributed back to the right disk by
  per-partition GUIDs. See [docs/backups.md](docs/backups.md).
- **A theory about the cause, and a fix for it.** The infamous Windows 24H2 upgrade
  damage on the main GPT is two bytes, and the arithmetic that produces them is reproducible.
  See [docs/corruption.md](docs/corruption.md).
- **It can tell you the partition table was never the problem.** A machine
  that boots to nothing may have an intact disk and an emptied `BootOrder`
  instead, so the tool shows the firmware's boot entries — including the ones
  that have fallen out of the list and gone invisible — alongside the loaders
  actually present on the ESPs, and can put a missing one back. The whole
  boot configuration is saved to the ESP before the first change.
  See [docs/boot.md](docs/boot.md).

## Getting it running

### 1. Download it

Grab `bootfixr.efi` and `SHA256SUMS` from the
[latest release](https://github.com/speed47/efi-boot-fixer/releases/latest).
The `continuous` prerelease is a rolling build of the default branch; prefer a
tagged release if one exists.

```sh
sha256sum -c SHA256SUMS
```

### 2. Copy it to the ESP

Mount the Deck's EFI system partition and drop the binary in:

```sh
cp bootfixr.efi /esp/EFI/bootfixr.efi
```

On SteamOS the ESP is normally already mounted at `/esp`. If the Deck no
longer boots, mount its ESP from another Linux machine or a live USB and copy
the file there — installing to the internal ESP is what means no external
media is needed *afterwards*, when you actually run it.

This is deliberately **not** registered as an NVRAM boot entry, because
SteamOS rewrites the boot order on update. Note that you can add it maually
using the tool itself if you want it.

### 3. Boot it

1. Shut the Deck down completely.
2. Hold **Volume Up (+)** and tap **Power**, keeping Volume Up held until the
   Boot Manager appears.
3. Choose **Boot From File**, then the EFI system partition (usually it's the
   one that is preselected), then `EFI`, then `bootfixr.efi`.

### 4. Use it

- **D-pad** moves, **A** chooses, **B** goes back or cancels.
- **View** (the two rectangles, left of the left stick) opens the Display config
  screen from anywhere: LEFT and RIGHT turn the picture, UP and DOWN change
  the text size. It starts up the right way round on a Deck, so this is only
  there if it comes out wrong or you want the text bigger.
- Start with **Check this machine** — it writes nothing, looks at everything,
  and ends by naming the menu that holds the fix for what it found.
- Then **Partition tables (GPT) → Back up both GPTs**, before anything that
  writes. DO NOT SKIP THIS STEP.
- In any case, actually writing requires the sequence **LEFT RIGHT LEFT RIGHT A**;
  any wrong press resets it, and B cancels with nothing written.

[docs/using.md](docs/using.md) walks through the screens.

## Building from source

Requires a Rust toolchain with the `x86_64-unknown-uefi` target:

```sh
rustup target add x86_64-unknown-uefi
make build          # -> crates/bootfixr/target/x86_64-unknown-uefi/release/bootfixr.efi
make                # list every target
```

See [docs/building.md](docs/building.md) for the details, and
[docs/testing.md](docs/testing.md) for the test and QEMU harnesses.

## Documentation

| | |
| --- | --- |
| [docs/using.md](docs/using.md) | the screens, the confirmation gate, what the colours mean |
| [docs/safety.md](docs/safety.md) | every refusal, and how writes are ordered |
| [docs/backups.md](docs/backups.md) | the snapshot format, restoring, choosing between snapshots |
| [docs/corruption.md](docs/corruption.md) | what the damage is, why it recurs, how to reproduce it |
| [docs/boot.md](docs/boot.md) | NVRAM boot entries, the load option format, finding loaders on the ESPs |
| [docs/input.md](docs/input.md) | what a Steam Deck's controls actually report to firmware |
| [docs/display.md](docs/display.md) | the rotated renderer, the baked font, the two backends |
| [docs/internals.md](docs/internals.md) | crate layout, drive identification, the expected partition set |
| [docs/building.md](docs/building.md) | building, installing, CI |
| [docs/testing.md](docs/testing.md) | host tests, the QEMU harness, the real-hardware fixture |

The baked font is DejaVu Sans Mono; its licence is in
[docs/FONT-LICENSE](docs/FONT-LICENSE).
