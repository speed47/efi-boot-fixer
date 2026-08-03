# Output: the panel is mounted sideways

The Deck's LCD is a portrait panel turned on its side. The firmware reports it
honestly — 800 across, 1280 down — and lays its text console out in those
coordinates, so everything printed through
`EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL` arrives on the glass a quarter turn
anticlockwise. The tool was usable that way only because a handheld can be
physically rotated to read it.

There is no fixing that through the text protocol: it hands out a character
grid and nothing underneath it. So the application also carries its own
renderer. It takes the framebuffer from `EFI_GRAPHICS_OUTPUT_PROTOCOL`, draws
characters from a font baked into the binary, and puts every pixel through a
rotation on the way out.

Both backends are always built, and one is chosen at startup:

| Condition | Backend |
| --- | --- |
| a graphics protocol with a CPU-addressable framebuffer | own renderer, rotated to suit the panel |
| `PixelBltOnly`, or no graphics protocol at all | the firmware's text console, as before |

Nothing above `crates/bootfixr/src/ui/term.rs` knows which one it got. The
menus, the reports and the confirmation gate are written once, against a
character grid with a cursor and sixteen colours, and drawn by either.

## Which way up, and how big

A framebuffer taller than it is wide means a panel mounted sideways, and the
correction is a quarter turn clockwise; anything else is taken at face value.
That guess is taken as the answer and the session opens on the menu.

Arguing with it is the Display screen, which **View** reaches from any screen
that waits for a press. LEFT and RIGHT turn the picture — controls that work
no matter which way round the text came out, which is the only reason the
screen can rescue a wrong guess. UP and DOWN step the text size, because how
big is comfortable depends on a particular person holding a particular panel
at whatever distance they hold it, and no amount of arithmetic settles that
here.

It used to be the first screen, on a six-second timer. That charged every
launch for a correction almost nobody needs, and still offered it only once —
so a guess that turned out wrong three screens in, or a report of device paths
that wanted more columns than the menu it came from, had no answer at all. A
button that works everywhere is the same rescue without the toll. The timer
went with it: there is nothing left to time out into.

The one screen that keeps View for itself is the snapshot list, where it opens
the selected record; its footer says so. On the firmware's own text console
the orientation and the font belong to the firmware, so there is no Display
screen and no footer offers one.

## The font

DejaVu Sans Mono, rasterised on the host by `tools/mkfont` into 8-bit coverage
bitmaps at three cell sizes and committed as generated Rust. Each glyph is
trimmed to its own bounding box, which halves the baked data; the whole font
costs about 41 KB. Its licence is in [FONT-LICENSE](FONT-LICENSE).

Which cell to use is chosen from the framebuffer. Taking the *largest* that
clears the 80x25 the menus were laid out against was the obvious rule and the
wrong one: on any screen clearing 80 columns by a little it picked the
coarsest size that fitted, leaving device paths truncated and reports
paginated that did not need to be. Aiming at a target line length instead
lands where it should:

| Framebuffer | Cell | Grid |
| --- | --- | --- |
| 1280x800 — a Deck, rotated | 12x24 | 106x33 |
| 800x600 — OVMF's default | 8x16 | 100x37 |
| 1920x1080 and up | 16x32 | 120x33 and up |

Only sizes that still clear 80x25 are offered, automatically or on request,
so no choice available on the display screen can leave the menus unable to
lay out.

`bootfixr-tiny.efi` (see [building.md](building.md)) is built with only the
12x24 cell compiled in, for a Deck whose ESP has no room to spare: dropping
8x16 and 16x32 saves about 28 KB before the binary is UPX-compressed on top
of that. It never offers a text-size choice, since there is nothing to step
to; the Display screen's UP/DOWN just report there is nothing further, same
as running out of sizes on the full binary.

## Repainting

The menus clear and redraw on every keypress, so `clear` does not touch a
pixel: it blanks a grid of cells, and the flush that follows repaints only the
cells whose contents actually changed. Moving the highlight bar costs two rows
rather than a screen, and there is no black flash under the cursor. The
framebuffer is filled outright only when the orientation or the text size
changes.
