# Using it

A full-screen D-pad menu with a highlight bar, because the hardware has no
keyboard. Cursor positioning and sixteen colours are all it needs, which is
why the same screens can be drawn either by the firmware's text console or by
the application's own renderer — see [display.md](display.md).

The selected item's description appears below the list, which doubles as
inline help:

```
EFI GPT Toolkit for Steam Deck
------------------------------------------------------------------
  version 0.1.0+3.g1a2b3c4
  launched from PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,...)/HD(1,GPT,...)

  Check GPT                                          <- highlighted
   Repair primary GPT from the backup
   Back up both GPTs to the ESP
   Restore GPTs from a saved backup
   Prevent recurrence (close the FirstUsableLBA gap)
   Boot entries (NVRAM)
   Exit
   Reboot

  Read both tables and report what is wrong.
  Writes nothing.
  D-pad = move    A = choose    B = exit    View = display
```

`View = display` is offered on every screen that waits for a press, and opens
the screen that turns the picture and changes the text size — see
[display.md](display.md). It is absent when the firmware's own text console is
drawing, which has neither to offer.

## The version line

A release built from a tag reports its version alone — `0.1.0`. Anything else,
including every continuous build, appends the commit it was compiled from as
semver build metadata: `0.1.0+3.g1a2b3c4` is three commits past `v0.1.0` at
`1a2b3c4`, and `0.1.0+g1a2b3c4` is a build made before any tag existed. A
`.dirty` on the end means the working tree had uncommitted changes.

That suffix is the only way to tell which build someone is running, since the
package version does not move between releases, so quote the whole line when
reporting anything. It is computed by `crates/gpttoolk/build.rs` at compile
time, and the same string is stored in snapshot metadata and written to
`efiprobe.log`.

## The disk picker

Choosing an operation opens a disk picker, which is where the identifying
detail lives:

```
Repair primary GPT
------------------------------------------------------------------
  Choose a disk. Removable media is not listed.
  [boot] carries the volume this program came from.

   Disk 1    96.0 MiB  [boot]
  Disk 2   931.5 GiB  [SteamOS]                      <- highlighted

  PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,...)
  1953525168 blocks x 512 B
  GPT: primary GPT is repairable from the backup
```

`[SteamOS]` is inferred from the table itself: an ESP plus either two Linux
root partitions, two `/var` partitions, or two names forming an `-A`/`-B`
pair. That structure is what makes a SteamOS install recognisable at a glance
and it survives any single partition being renamed. A drive model is shown
when the firmware will give one — see [internals.md](internals.md).

## Reports and the confirmation gate

Reports paginate with the D-pad, since a Deck cannot scroll back. Up and down
move three lines at a time, not one — the D-pad repeats at about 1.8/s while
held, so a line per press makes a long report a chore — and left and right
move a whole screen. Writes are authorised by a fixed sequence rather than a
held button:

```
  To authorise this, press in order:

     LEFT  RIGHT  LEFT  RIGHT  A
     [x]   [x]    [ ]   [ ]    [ ]

  next: LEFT
  B = cancel, nothing is written
```

Any wrong press resets it; B cancels outright. A sequence was chosen over
hold-to-confirm because it depends only on discrete presses: it does not rely
on the firmware's auto-repeat, and a buffered burst of repeats cannot walk
through it. Every screen that asks a question drains queued input first, for
the same reason. See [input.md](input.md) for where those two constraints
come from.

## What the colours mean

Colour carries meaning rather than decoration. Each line is tagged by the code
that writes it — `gptcore::style::Style` — with the mapping to actual colours
living in the application:

| Style | Colour | Used for |
| --- | --- | --- |
| `Title` | white | captions introducing a block |
| `Normal` | light grey | body text |
| `Dim` | dark grey | device paths, column headings, provenance |
| `Good` | light green | healthy, verified, succeeded |
| `Warn` | yellow | needs attention, caveats worth reading |
| `Bad` | light red | damage, refusal, failure |
| `Key` | light cyan | the value you must actually take in: an LBA about to be overwritten, the disk you are pointed at |

The UEFI side never decides a colour by looking at the text, so rewording a
message cannot silently change what it looks like. In the confirmation gate
each progress box is coloured individually — completed steps green, the one
being waited for cyan, the rest dim.
