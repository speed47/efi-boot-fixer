# Boot entries in NVRAM

A wrecked partition table is one way to end up with a Deck that will not
boot. An emptied or reordered `BootOrder` is the other, and it looks
identical from the outside: the machine powers on and goes nowhere. This
half of the tool is about the second one.

Everything described here is **read-only**. Nothing in this build writes to
NVRAM. Registering a loader and changing the default are the next step; see
the end of this file for what that will involve.

## What the firmware actually stores

Four global variables matter:

| Variable | What it is |
| --- | --- |
| `Boot####` | One boot entry, an `EFI_LOAD_OPTION`. `####` is four hex digits. |
| `BootOrder` | An array of `u16` slot numbers, in the order the firmware tries them. |
| `BootCurrent` | The slot this boot came from. |
| `BootNext` | A one-shot override, consumed by the next boot. |

An `EFI_LOAD_OPTION` (UEFI spec §3.1.3) is four fields, three of them
variable-length:

```
u32   Attributes            LOAD_OPTION_ACTIVE, _HIDDEN, category bits
u16   FilePathListLength    length in bytes of the device path below
CHAR16 Description[]        NUL-terminated UCS-2
u8    FilePathList[]        exactly FilePathListLength bytes
u8    OptionalData[]        everything remaining
```

The description's length is implied by its terminator and the device path's
length is declared in the head, so *both* boundaries are positions where a
truncated variable can send a parser off the end of the buffer. A firmware
that ran out of variable store mid-write leaves exactly that. `bootopt`
checks each boundary against the buffer it was actually handed and reports
the mismatch rather than reading past it; `tests/bootopt.rs` has a case per
boundary.

## Why the device path is not parsed in `gptcore`

`gptcore` keeps `device_path` as opaque bytes and never looks inside. Two
reasons, and the second is the one that matters:

- Rendering a device path needs the firmware's whole vocabulary of hardware
  — PCI topology, NVMe namespaces, USB ports, firmware volumes — and the
  `uefi` crate already implements it. Reimplementing it here to display a
  string would be a large amount of code with no independent oracle.
- `gptcore` is the part that can be tested on the host. Keeping it free of
  anything firmware-shaped is what makes the byte format testable at all.

The application renders the path and hands the text back in when it wants a
line drawn. `bootopt::render_flags` does the rest, so the decision about
what a line *means* — and therefore its colour — still lives in `gptcore`,
which is the rule the rest of the codebase follows.

## Why entries are enumerated, not read from `BootOrder`

It would be less code to read `BootOrder` and fetch the slots it names. It
would also hide the failure this screen exists to show.

A `Boot####` can be present in NVRAM and absent from `BootOrder`. When that
happens the firmware's own boot menu does not offer it, and the entry is
invisible by every means the machine gives you — which is precisely the
state someone is in when they say the OS "disappeared" from the boot menu
after an update. So `nvram::read` walks the whole variable store with
`GetNextVariableName`, keeps every global whose name is `Boot` followed by
exactly four hex digits, and the view screen shows those under **Present,
but not in the boot order**.

The strictness of that name match is load-bearing in the other direction:
`BootOrder`, `BootNext`, `BootCurrent` and `BootOptionSupport` all begin
with `Boot`, and a lenient match sweeps them into the entry list.

## Finding loaders on the ESPs

`espscan` looks at every partition whose GPT type GUID is the EFI System
Partition, skipping removable media — a boot entry pointing at a USB stick
breaks the moment the stick comes out, and that matches the refusal list in
[safety.md](safety.md). This is a different job from [esp.rs](../crates/gpttoolk/src/esp.rs),
which reads and writes the tool's own backups on the volume it was launched
from and only there.

Probing is two explicit lists rather than a guessed vendor table:

- a one-level sweep of the directories under `\EFI`, reporting **every**
  `.efi` file in each;
- plus `\EFI\Microsoft\Boot\bootmgfw.efi`, which is two levels down and so
  out of the sweep's reach.

A filename table decides what a binary is *called* — `grubx64.efi` is GRUB,
`steamcl.efi` is the SteamOS chainloader — but a file missing from that
table is still listed, as "unrecognised EFI binary". This is deliberate.
An earlier version of the repair path matched partitions against a guessed
type-GUID table and refused to touch the exact disk it was written for; a
loader that goes unlisted because nobody added its filename would be the
same mistake pointed at a different structure.

### Deciding whether an entry already points at a file

Comparing whole device paths byte for byte is too strict. Firmware stores
both the full path, carrying the controller topology, and the short
`HD()/File()` form, for the same target. What actually pins a binary down is
the pair (partition GUID, file path), and both spellings carry it. An entry
with no hard-drive node at all — a PXE boot, the firmware's setup
application, the built-in shell — matches nothing, and is reported as
matching nothing rather than guessed at.

## Reading NVRAM from the host

`tools/dump-efivars.py` walks an OVMF varstore image from outside the
machine:

```sh
tools/dump-efivars.py build/qemu/vars.fd              # list what is there
tools/dump-efivars.py build/qemu/vars.fd Boot0000 > entry.bin
```

The fixtures under `crates/gptcore/tests/data/boot/` were captured this way
from a varstore OVMF had populated on its own, and `tests/bootopt.rs`
asserts they re-encode byte for byte. They are worth more than a hand-built
fixture because this crate did not produce them: `Boot0000` is OVMF's
`UiApp`, hidden and flagged as an application, with a device path made of
firmware-volume nodes and no hard-drive node — the case the ESP matching has
to answer "no" for.

`make qemu SCRIPT=bootentries` walks both screens under OVMF. The harness
gives each run a fresh copy of the varstore, so `Boot0000`, `Boot0001` and a
two-entry `BootOrder` are always there to render.

## What writing will involve

Not in this build. When it lands:

- The whole boot configuration gets saved to `\GPTTOOLK\boot.NNN` on the ESP
  before the session's first NVRAM write, automatically. A mangled
  `BootOrder` on a machine with no keyboard is the situation this tool is
  for, and it should not be able to create one it cannot undo.
- Every write goes behind the same five-press confirmation gate as a block
  write, showing the decoded variable first.
- New entries take the **lowest free** slot. That is deliberately unlike
  `backup::next_name`, which counts up from the highest and never fills a
  gap: reusing a snapshot *filename* would destroy a backup nobody can get
  back, whereas a freed boot slot holds nothing at all. Firmware and
  `efibootmgr` both reuse the lowest.
