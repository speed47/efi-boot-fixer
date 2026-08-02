# Input: the Deck has no keyboard

Everything about the interface follows from one constraint: there is no
built-in keyboard, Bluetooth is not available in the firmware environment, and
few people have a USB-C keyboard to hand. The buttons, sticks, trackpads and
touchscreen are the only realistic inputs, and how the firmware exposes them
is not something to guess at.

`efiprobe.efi` answers that empirically. It enumerates the input protocols the
firmware publishes and logs every event it sees:

| Protocol | What it would give us |
| --- | --- |
| `EFI_SIMPLE_TEXT_INPUT_PROTOCOL` | scan codes and Unicode chars, i.e. buttons mapped to keys |
| `EFI_SIMPLE_POINTER_PROTOCOL` | relative motion from trackpads or a mouse |
| `EFI_ABSOLUTE_POINTER_PROTOCOL` | the touchscreen, with its coordinate range |

Everything goes to `efiprobe.log` **on the ESP it was launched from**, flushed
after every line, as well as to the screen. The screen scrolls and cannot be
copied off the device; the file can be read from Linux afterwards, and cutting
the power keeps whatever was logged up to that moment. The probe walks 30
guided steps at 6 seconds each plus a 20-second free-form phase, then exits on
its own after roughly 200 seconds, because without a keyboard there may be no
way to tell it to stop.

```sh
make probe-esp ESP=/path/to/esp     # installs EFI/efiprobe.efi
# boot menu -> boot from file -> efiprobe.efi
# press each control in turn, then read EFI/../efiprobe.log
```

Verified end to end under OVMF, including recovering the log file from the ESP
afterwards.

## What a Steam Deck actually reports

Measured on real hardware, firmware `Valve rev 0x10033`, UEFI 2.70. The raw
capture is in [efiprobe-deck.log](efiprobe-deck.log).

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
