# AGENTS.md

Notes for anyone — human or agent — changing this repository. The user-facing
description lives in [README.md](README.md); the reasoning behind each part
lives under [docs/](docs/). This file is only the things that are easy to get
wrong.

## Repository layout

```
crates/gptcore/      no_std, no UEFI dependency - parsing, validation, planning
crates/gptcore/tests/  host tests; sgdisk is the independent oracle
crates/bootfixr/     the EFI_APPLICATION (its OWN workspace; UEFI target only)
  src/ui/            menus, and the two backends they can be drawn on
  src/gfx/           framebuffer, rotation, baked font, character console
  src/nvram.rs       reading Boot####/BootOrder out of the variable store
  src/secureboot.rs  the Secure Boot flags and key databases, read-only
  src/smbios.rs      copying the SMBIOS table out of firmware memory
  src/store.rs       our own files, on the ESP and on removable media
  src/espscan.rs     finding bootloaders on the ESPs (not src/store.rs)
  src/diag.rs        the firmware half of the diagnostic report
tools/               image builders, the QEMU harness, the font rasteriser
docs/                the long-form reasoning that used to be in the README
```

`gptcore` performs no I/O and knows nothing about firmware: it reads through a
`BlockDevice` trait and takes CRC-32 as an injected dependency. Keep it that
way — it is why the logic that decides what to overwrite can be tested on the
host. `repair::plan` returns an ordered list of steps rather than writing
anything; that ordering is an assertable data structure, not a comment.

## Commands

```sh
make                # list every target
make check          # fmt-check + clippy + test + build; exactly what CI runs
make test           # host tests (requires gdisk installed)
make build          # both EFI binaries, release
make qemu SCRIPT=repair     # boot under OVMF and drive the menus over serial
```

- `make check` before declaring anything done. Clippy runs with
  `-D warnings` in both workspaces.
- `rustfmt.toml` sets `max_width = 100` and `use_small_heuristics = "Max"`
  deliberately; the default heuristics explode short expressions.

## Traps

**Two workspaces.** `crates/bootfixr` is a separate workspace and only builds
for `x86_64-unknown-uefi`. A bare `cargo build` at the root does not touch it,
and a bare `cargo test` cannot compile it. The Makefile invokes it separately
with an explicit `--target`, and prefers `~/.cargo/bin/cargo`, because distro
cargo packages generally ship no std for the UEFI target. Use the Makefile.

**`sgdisk -v` lies about a wrecked main GPT.** It transparently falls back to
the secondary and reports on the table it loaded, printing "No problems found".
Any check that uses it as an oracle must also require the absence of
`Caution`/`Warning`/`ERROR` text.

**OVMF repairs the main GPT before the application runs.** EDK II's
`PartitionDxe` calls `PartitionRestoreGptTable()` at connect time. Under QEMU
the `zero-header` and `zero-all` corruption modes are therefore already fixed
by the time the tool loads, and `Main GPT: OK` is correct, not a bug. Use
`CORRUPTION=bad-mbr` to exercise the write path under firmware; the repair
itself is covered by host tests where no firmware sits in the way.

**Matching is by partition name, never by type GUID.** An earlier version
required both, with a guessed type table, and the tool refused to repair the
exact disk it was written for. Type GUIDs are compared and reported but must
never be fatal. See [docs/internals.md](docs/internals.md).

**The header CRC of the real corruption is valid.** A checker that validates
only the header CRC calls the affected disk healthy. What catches it is the
entry-array CRC plus an explicit check that a main GPT header points at LBA 2.
`tests/deck_corrupt.rs` asserts the *absence* of a header-CRC defect
specifically so this check cannot be "simplified" away.

**Boot entries are enumerated, never read out of `BootOrder`.** Following
the order would hide a `Boot####` that has fallen out of it — which is the
exact failure the screen exists to show, and is invisible by every other
means the machine offers. The name match is strict (`Boot` plus exactly four
hex digits) because `BootOrder`, `BootNext` and `BootCurrent` would otherwise
land in the entry list. See [docs/boot.md](docs/boot.md).

**"Never removable media" is a rule about disks, not about files.** The
repair targets exclude removable and read-only *block devices*; `store.rs`
deliberately offers removable *volumes* as backup destinations, through a
filesystem. Under QEMU that path needs `USB=1`, and specifically
`-device usb-storage,...,removable=on`: without `removable=on` QEMU leaves
the RMB bit clear in the SCSI INQUIRY, EDK II marks the media fixed, and the
destination menu never appears. See [docs/backups.md](docs/backups.md).

**`GptPartitionEntry` is a packed struct.** Its fields must be copied out
(`{ gpt.partition_type_guid }`) before being compared; taking a reference to
one does not compile, and would be UB if it did.

**The version comes from `build.rs`, not `CARGO_PKG_VERSION`.** Use
`env!("BOOTFIXR_VERSION")`: it is the package version with the `git describe`
commit appended for anything that is not an exact tag, which is the only way a
continuous build in the wild can be traced back to a commit. CI checks that the
staged binaries contain the commit hash, and clones with `fetch-depth: 0`
because a shallow one cannot describe. See [docs/using.md](docs/using.md).

**`font_data.rs` is generated and committed.** Do not hand-edit it; `make
font` re-bakes it from DejaVu Sans Mono. A normal build needs no font
installed.

**Input is buffered and auto-repeats.** Holding A yields ~10.5 events/s and
bursts arrive late, so a confirmation gate cannot count events; it requires
discrete presses in sequence, and every screen that asks a question drains
queued input first. See [docs/input.md](docs/input.md).

## Invariants worth protecting

- Entry arrays are written and flushed **before** the header that points at
  them, in both repair and restore. A power cut must not leave a valid header
  describing garbage. The NVRAM side follows the same rule: `Boot####` is
  written before the `BootOrder` naming it, asserted in `tests/bootwrite.rs`.
- The boot configuration is saved to `\BOOTFIXR\boot-NNN.bkp` before the
  session's first NVRAM write. Its variables are stored as opaque bytes and
  never re-encoded — an entry this build cannot parse is the one most worth
  copying exactly.
- A GPT repair saves `\BOOTFIXR\gpt-NNN.bkp` to the ESP by itself, labelled
  `automatic, before repair` in the metadata section, between the review
  page and the confirmation gate. A failure to save it is a question with
  the consequence spelled out, never a silent skip and never a hard
  refusal. Snapshot labels are a metadata key (`backup::META_LABEL`), not a
  format version — older builds skip unknown keys by design.
- `bootcfg::plan_restore` follows the same rule on the way back: every
  `Boot####` before the `BootOrder` naming it, `BootNext` last of all, and
  nothing deleted. Asserted in `tests/bootwrite.rs`.
- Boot slots are allocated **lowest free**; snapshot filenames count up and
  never fill a gap. The rules are opposite on purpose: a filename holds the
  only copy of something, a boot slot holds nothing.
- Nothing writes without the five-press confirmation sequence, and the exact
  LBAs to be written are shown first.
- The refusal list in [docs/safety.md](docs/safety.md) is a feature list, not
  incidental. Removing a refusal is a product decision.
- The boot disk is deliberately **not** excluded — that was reversed on
  purpose. Exclusive opens on it may cut off ESP access, so file operations
  happen before block writes.
- Snapshot `decode` accepts version 1 and version 2; only `encode` moved on. A
  snapshot a later build refuses is worthless.
- Colour is decided where a line is written (`gptcore::style::Style`), never
  by inspecting text in the UEFI layer.
- SMBIOS serial numbers, asset tags and the system UUID are **masked** in
  the diagnostic report (`gptcore::smbios::mask`), keeping the length and
  three characters. Nothing in the tool may describe the file as anonymous:
  partition GUIDs, `UsbWwid()` path nodes and firmware-built boot entry
  descriptions still identify the machine. See [docs/report.md](docs/report.md).
- The diagnostic report is **not** wrapped to any width, unlike every screen.
  Its reader is a forum thread, not a 7-inch panel, and a device path broken
  across three lines cannot be searched for or diffed. See
  [docs/report.md](docs/report.md).
- Snapshot names count up from the highest present and never fill a gap. So
  do diagnostic reports (`diag-NNN.txt`), which share the directory and the
  numbering pass with them.
- A snapshot written to two destinations gets **one name**, numbered from
  what is in use on every volume the tool can see — not just the
  destinations, or saving to one volume now and another later yields two
  different files sharing a name. A destination whose directory cannot be
  listed is dropped from the write rather than counting as empty *or*
  failing the whole thing; a destination that fails the write does not stop
  the others; both outcomes reach the screen.

## Conventions

- **"Backup" only ever means a file we saved.** The two GPTs on a disk are
  the **main GPT** (LBA 1) and the **secondary GPT** (last LBA) —
  `Analysis::main` / `Analysis::secondary`, `Role::Main*` / `Role::Secondary*`,
  `Verdict::MainRepairable`. "Repair from backup" was ambiguous between the
  end-of-disk copy and a snapshot in `\BOOTFIXR\`, so it is not said any more.
- **A menu row carries its action, never its index.** `Menu<A>` in `main.rs`
  pairs each row with an enum variant, so reordering a menu is one edit
  rather than an edit to a list and a matching edit to a `match` on numbers
  that nothing checks against it. The QEMU walks in `tools/run-qemu.sh` do
  count presses, and are the thing to update when a menu changes.
- Commit subjects are short prose in the imperative, describing the change's
  point rather than its mechanics: "Refuse a hybrid MBR in Prevent too, not
  just in Repair", "Cut the parts of gptcore nothing calls".
- Comments explain why, not what, and are used sparingly. Match the density of
  the surrounding file.
- The QEMU harness is not in CI, on purpose: serial-driven menus depend on
  boot timing and would be flaky. Run it locally.
- When something is measured on real hardware rather than assumed, say so —
  the docs mark measurements (firmware `Valve rev 0x10033`, UEFI 2.70) as
  such, and that distinction has already been load-bearing once.
