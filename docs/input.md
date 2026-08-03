# Input: the Deck has no keyboard

Everything about the interface follows from one constraint: there is no
built-in keyboard, Bluetooth is not available in the firmware environment, and
few people have a USB-C keyboard to hand. The buttons, sticks, trackpads and
touchscreen are the only realistic inputs, and how the firmware exposes them
is not something to guess at.

That was answered empirically, by walking a scripted list of controls on real
hardware and logging every event the firmware's input protocols reported for
each one:

| Protocol | What it gives us |
| --- | --- |
| `EFI_SIMPLE_TEXT_INPUT_PROTOCOL` | scan codes and Unicode chars, i.e. buttons mapped to keys |
| `EFI_SIMPLE_POINTER_PROTOCOL` | relative motion from trackpads or a mouse |
| `EFI_ABSOLUTE_POINTER_PROTOCOL` | the touchscreen, with its coordinate range |

## What a Steam Deck actually reports

Measured on real hardware, firmware `Valve rev 0x10033`, UEFI 2.70. The raw
capture is in [steamdeck-input.log](steamdeck-input.log).

| Control | Event |
| --- | --- |
| A | unicode `0x000D` (CR) |
| QAM (three dots) | unicode `0x000D` (CR) — **indistinguishable from A** |
| B | scan `0x17` (ESCAPE) |
| Menu / burger | scan `0x17` (ESCAPE) — **indistinguishable from B** |
| D-pad up / down / left / right | scan `0x01` / `0x02` / `0x04` / `0x03` |
| View (two rectangles) | unicode `0x0009` (TAB) |
| L2 trigger | `SimplePointer[1]` **right** button |
| R2 trigger | `SimplePointer[1]` **left** button |
| Right trackpad | `SimplePointer[1]` relative dx/dy, click = left button |
| Left trackpad | `SimplePointer[1]` dz only, i.e. a scroll wheel |
| X, Y | nothing |
| L1, R1 bumpers | nothing |
| L4, L5, R4, R5 back buttons | nothing |
| Both sticks: click and movement | nothing |
| STEAM button | nothing |
| Touchscreen: tap and drag | nothing |

So the usable set is: **CR**, **ESCAPE**, **four D-pad scan codes**, **TAB**,
and a relative pointer with two buttons and a scroll axis.

TAB — the View button — is the only press beyond a D-pad and two buttons that
the hardware can distinguish, which makes it the one place to put anything
that is not "move, choose, go back". It opens the Display screen from every
screen that waits for a press, because wanting it means being unable to read
the screen, and that can happen anywhere. The snapshot list is the single
exception: there View opens the selected record, and its footer says so
instead.

Three results worth calling out:

**Keys auto-repeat while held.** Holding A for six seconds produced 63 CR
events, about 10.5/s. That makes hold-to-confirm possible, which matters:
`EFI_SIMPLE_TEXT_INPUT_PROTOCOL` reports presses with no key-release event, so
without auto-repeat there would be no way to detect a held button at all.
D-pad DOWN also repeats but far slower, around 1.8/s.

**Input is buffered.** The step after the "hold A" test recorded 7 stray CR
events before its own. A confirmation gate therefore cannot simply count
events; it has to require them to keep *arriving*, and reset on a gap.

**The touchscreen is not available.** An `EFI_ABSOLUTE_POINTER_PROTOCOL` is
published with an 0..65536 range, but neither a tap nor a drag produced a
single event. No touch-target interface is possible.
