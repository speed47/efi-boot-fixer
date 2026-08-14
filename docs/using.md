# Using it

A full-screen D-pad menu with a highlight bar, because the hardware has no
keyboard. Cursor positioning and sixteen colours are all it needs, which is
why the same screens can be drawn either by the firmware's text console or by
the application's own renderer — see [display.md](display.md).

The selected item's description appears below the list, which doubles as
inline help:

```
EFI Boot Fixer for Steam Deck - github.com/speed47/efi-boot-fixer
------------------------------------------------------------------
  version 1.2.0+3.g1a2b3c4 compiled 2026-08-14 16:36
  launched from PciRoot(0x0)/Pci(0x2,0x0)/NVMe(0x1,...)/HD(1,GPT,...)

  Check this machine [read only]                     <- highlighted
   Generate a diagnostic report [read only]
   Partition tables (GPT) ...
   Boot entries (NVRAM) ...

   Boot a loader now (chainloading)
   Reboot
   Shutdown
   Exit to the firmware

  This will check every disk's partition table, the
  firmware's boot list, and what is on the EFI System
  Partitions (ESPs).
  ----------------------------------------------------------
  [D-pad] move   [A] choose   [View] configure display   [B] exit
```

The title carries the project's URL, so a photograph of a screen is enough
to find the tool it came from. "for Steam Deck" is in it only when SMBIOS
says this really is a Deck: the code has no hardware dependency left, and
naming somebody else's laptop after a handheld reads as a tool that has
misunderstood the machine it is looking at.

## The shape of the menus

Two diagnostics, two doors, and the ways out. Operations are grouped by what
they act on, and each group is ordered by what it costs to be wrong:

```
Check this machine [read only]
Generate a diagnostic report [read only]

Partition tables (GPT)                      Boot entries (NVRAM)
  Check a disk's GPT                          View the boot entries
  Back up both GPTs to a file                 Scan the ESPs for bootloaders
  Restore GPTs from a saved backup            Register a bootloader
  Repair main GPT from the secondary          Set the default boot entry
  Prevent recurrence (experimental)           Boot something once (next boot only)
                                              Back up the boot configuration
                                              Restore boot configuration from backup

Boot a loader now (chainloading)
Reboot / Shutdown / Exit to the firmware
```

A separator sets those last four rows apart. They are not things the program
does to a disk, they are ways to leave: to one specific program, restarted,
powered off, or handed back to the firmware in general. "Boot a loader now"
belongs with them for that reason and not with the NVRAM screens, even
though it is the ESP scan that finds what it can start — it writes nothing
anywhere and this session is over the moment it works. See
[boot.md](boot.md#booting-a-loader-immediately).

Read-only screens come first in both submenus, then anything that writes.
On the GPT side, backing up sits above the three operations that overwrite a
table because that is the order the two should be done in, and a menu is the
cheapest place to say so. The NVRAM side follows the same rule: backing up
sits above restoring, and below the three operations that change NVRAM
directly, since a manual backup is not required before them — a backup is
taken automatically before this session's first NVRAM write regardless of
whether this screen was ever visited.

**Start with "Check this machine".** It reads every disk's partition table,
the firmware's boot list and the loaders installed on the ESPs, writes
nothing, and ends by naming the submenu that holds the fix for whatever it
found. It exists because the question someone arrives with is not "is my GPT
valid" but "why will this thing not boot", and answering that used to mean
knowing in advance which half of the tool to look in.

**"Generate a diagnostic report" is the same reading, written down in full.** The
screen above it is deliberately short, because a screen that says everything
says nothing on a 7-inch panel; the report leaves nothing out, and goes to a
text file meant to be attached to a forum post. See [report.md](report.md).

Nothing is more than two rows deep: the row that opens a submenu, then the
operation. The rows ending in `...` are the doors; the rest do something.

`[View] configure display` is offered on every screen that waits for a press,
and opens the screen that turns the picture, changes the text size and
chooses a screen resolution — see [display.md](display.md). It is absent when
the firmware's own text console is drawing, which has none of the three to
offer.

## The version line

A release built from a tag reports its version alone — `1.2.0`. Anything else,
including every continuous build, appends the commit it was compiled from as
semver build metadata: `1.2.0+3.g1a2b3c4` is three commits past `v1.2.0` at
`1a2b3c4`, and `1.2.0+g1a2b3c4` is a build made before any tag existed. A
`.dirty` on the end means the working tree had uncommitted changes.

That suffix is the only way to tell which build someone is running, since the
package version does not move between releases, so quote the whole line when
reporting anything. It is computed by `crates/bootfixr/build.rs` at compile
time, and the same string is stored in snapshot metadata.

The build date sits next to it, and answers a question the version cannot
when somebody is holding a binary they copied onto an ESP months ago and no
longer remembers where from.

## The buttons the footer names

The footer is written for the hardware it is running on. On a Deck it names
the buttons — `[A]`, `[B]`, `[View]`, `[D-pad]` — and everywhere else the
same four hints come out as `[Enter]`, `[Escape]`, `[Tab]` and `[Arrows]`,
which is what a keyboard in front of a desktop firmware actually has. It is
the same test that puts "for Steam Deck" in the title: SMBIOS, read once at
startup. Promising a D-pad to somebody at a keyboard is a footer that has to
be decoded rather than read, and this tool is used by people who are already
having a bad day.

Every transcript in this document is written in the Deck's spelling, since
that is the hardware the screens were laid out for.

## The disk picker

Choosing an operation opens a disk picker when there is more than one disk,
which is where the identifying detail lives:

```
Repair main GPT
------------------------------------------------------------------
  Choose a disk. Removable media is not listed.
  [boot] carries the volume this program came from.

   Disk 1    96.0 MiB  [boot]
  Disk 2   931.5 GiB  [SteamOS]                      <- highlighted

  PciRoot(0x0)/Pci(0x3,0x0)/NVMe(0x1,...)
  1953525168 blocks x 512 B
  GPT: main GPT is repairable from the secondary GPT
```

`[SteamOS]` is inferred from the table itself: an ESP plus either two Linux
root partitions, two `/var` partitions, or two names forming an `-A`/`-B`
pair. That structure is what makes a SteamOS install recognisable at a glance
and it survives any single partition being renamed. A drive model is shown
when the firmware will give one — see [internals.md](internals.md).

**One disk is taken without asking.** A menu of one is not a choice, and on
a machine with a single internal drive — the normal case for the hardware
this was written for — every operation used to open with a press that could
only mean "yes, that one".

What makes that safe is the line below the title. From the moment a disk is
chosen, by hand or by there being nothing else, every screen the operation
draws names it:

```
Authorise write
  Disk 1   931.5 GiB  [boot]  [SteamOS]  KXG60ZNV1T02
------------------------------------------------------------------
  This REWRITES the partition table on this disk.
```

It is held by a guard in the application rather than written into each
screen's body, so an operation that gives up early cannot leave the next
screen naming the wrong disk. Nothing is ever authorised without the target
in front of you.

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
  ----------------------------------------------------------
  [B] cancel, nothing is written
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

Two colours are not in that table because they are not a `Style` at all: the
cyan bar on the selected row of a menu, and the **light magenta** the key
hints use for button names — `[A]`, `[B]`, `[View]`, `[D-pad]`, or their
keyboard spellings on anything that is not a Deck. Nothing else on any screen
names a control the operator can press, so that colour means exactly one
thing. The hints sit under a rule at the bottom of every screen; they used to
be dim throughout, which read as chrome and got skipped, taking `[View]` with
it.

The UEFI side never decides a colour by looking at the text, so rewording a
message cannot silently change what it looks like — the hints are held as a
button and an action separately, for the same reason. In the confirmation
gate each progress box is coloured individually — completed steps green, the
one being waited for cyan, the rest dim.
