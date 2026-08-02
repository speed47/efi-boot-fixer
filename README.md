# EFI GPT Toolkit for Steam Deck

A UEFI application for inspecting, repairing, backing up and restoring GUID
partition tables, targeting a Steam Deck that dual-boots Windows and SteamOS.

It is meant for a machine that will no longer boot: it runs from the ESP under
the firmware's "boot from file" menu, needs no keyboard, no USB stick and no
working operating system, and can repair the very disk it was launched from.
That works even with the primary table destroyed, because the Deck firmware
falls back to the backup GPT for partition enumeration. The Linux kernel does
not fall back without the `gpt` cmdline option, which is why `steamcl.efi`
loads but ultimately fails to boot. Symptoms are either:

- Black screen when booting SteamOS, even manually through `steamcl.efi`
- Dropping you to a `grub>` prompt after attempting to boot
- Verbose boot logs ending in `ERROR: Mounting /dev/disk/bypartuuid/<UUIDOFYOURPARTITION> failed.`

## Features

Five operations, all driven with the D-pad:

| Operation | Writes | What it does |
| --- | --- | --- |
| Check GPT | never | reads both tables and reports every defect |
| Repair primary GPT | disk | rebuilds a corrupt primary from the backup table |
| Back up both GPTs | ESP file | snapshots both tables to `\GPTTOOLK\` on the ESP |
| Restore GPTs | disk | writes a saved snapshot back |
| Prevent recurrence | disk | closes the `FirstUsableLBA` gap that causes the damage |
| Boot entries (NVRAM) | NVRAM | shows the firmware's boot list and the loaders on the ESPs; registers one, sets the default, or boots one once |

- **No keyboard needed.** The Deck's buttons are the only input; the menus,
  reports and prompts are built for a D-pad and two buttons.
  See [docs/input.md](docs/input.md).
- **Right way up.** The Deck's panel is mounted sideways, so the application
  carries its own framebuffer renderer and rotates every pixel on the way out.
  See [docs/display.md](docs/display.md).
- **Nothing is written by accident.** Every operation that touches a disk
  shows exactly which LBAs it will overwrite and then requires a five-press
  confirmation sequence. See [docs/using.md](docs/using.md).
- **It refuses more than it accepts.** No removable or read-only media, no
  hybrid MBRs, no implausible backup tables, no mismatched snapshots.
  See [docs/safety.md](docs/safety.md).
- **Snapshots you can still read years later.** Structured, checksummed,
  self-describing archives on the ESP, attributed back to the right disk by
  per-partition GUIDs. See [docs/backups.md](docs/backups.md).
- **A theory about the cause, and a fix for it.** The damage is two bytes, and
  the arithmetic that produces them is reproducible.
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

Grab `gpttoolk.efi` and `SHA256SUMS` from the
[latest release](https://github.com/speed47/efi-gpt-toolkit/releases/latest).
The `continuous` prerelease is a rolling build of the default branch; prefer a
tagged release if one exists.

```sh
sha256sum -c SHA256SUMS
```

### 2. Copy it to the ESP

Mount the Deck's EFI system partition and drop the binary in:

```sh
cp gpttoolk.efi /esp/EFI/gpttoolk.efi
```

On SteamOS the ESP is normally already mounted at `/boot/efi`. If the Deck no
longer boots, mount its ESP from another Linux machine or a live USB and copy
the file there — installing to the internal ESP is what means no external
media is needed *afterwards*, when you actually run it.

This is deliberately **not** registered as an NVRAM boot entry, because
SteamOS rewrites the boot order on update.

### 3. Boot it

1. Shut the Deck down completely.
2. Hold **Volume Up (+)** and tap **Power**, keeping Volume Up held until the
   Boot Manager appears.
3. Choose **Boot From File**, then the EFI system partition, then `EFI`, then
   `gpttoolk.efi`.

Secure Boot must be off, as the binary is unsigned. It is off by default on
the Deck.

### 4. Use it

- **D-pad** moves, **A** chooses, **B** goes back or cancels.
- The first screen offers to rotate the picture and change the text size; it
  continues on its own after six seconds if you press nothing.
- Start with **Check GPT** — it writes nothing and tells you what is wrong.
- Then **Back up both GPTs**, before anything that writes.
- Writing requires the sequence **LEFT RIGHT LEFT RIGHT A**; any wrong press
  resets it, and B cancels with nothing written.

[docs/using.md](docs/using.md) walks through the screens.

## Building from source

Requires a Rust toolchain with the `x86_64-unknown-uefi` target:

```sh
rustup target add x86_64-unknown-uefi
make build          # -> crates/gpttoolk/target/x86_64-unknown-uefi/release/gpttoolk.efi
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
