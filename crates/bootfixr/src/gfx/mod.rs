//! Drawing to the firmware's framebuffer, in whatever orientation the
//! panel needs.
//!
//! This exists because of one hardware fact: the Steam Deck's LCD is a
//! portrait panel mounted sideways. The firmware reports it honestly —
//! 800 across, 1280 down — and lays its text console out in those
//! coordinates, so everything the console prints arrives on the glass
//! rotated a quarter turn. The tool stayed usable only because a handheld
//! can be physically turned to read it.
//!
//! `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL` offers no way to fix that: it gives a
//! character grid and nothing underneath it. So this module goes to the
//! Graphics Output Protocol instead, takes the framebuffer, and puts every
//! pixel through a rotation on the way out. Costing us a font, which is
//! baked in by `tools/mkfont`, and a text console, which is
//! [`console`].
//!
//! The layering is deliberate:
//!
//! * [`Framebuffer`] knows about pixel formats and rotation, nothing else.
//! * [`console::Console`] knows about cells, cursors and dirty regions.
//! * `ui` knows about menus and never learns which of the two backends it
//!   is talking to.

pub mod console;
pub mod font;
// The `tiny` feature only reaches SMALL and LARGE through this module, not
// through anything crate-visible, so dropping them leaves rustc unable to
// see they are dead by design rather than by accident.
#[cfg_attr(feature = "tiny", allow(dead_code))]
mod font_data;

use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

/// A colour, before it is packed into whatever the framebuffer wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// How far the logical image is turned clockwise on its way to the panel.
///
/// [`Rotation::Cw90`] is the Steam Deck's case, and the reason this module
/// exists. The other variants cost nothing to support and mean an operator
/// facing an unexpected panel can turn the picture until it reads, rather
/// than being stuck with whatever was guessed for them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Half,
    Ccw90,
}

impl Rotation {
    /// The next orientation, for the picker at startup.
    pub const fn next(self) -> Self {
        match self {
            Rotation::None => Rotation::Cw90,
            Rotation::Cw90 => Rotation::Half,
            Rotation::Half => Rotation::Ccw90,
            Rotation::Ccw90 => Rotation::None,
        }
    }

    pub const fn previous(self) -> Self {
        self.next().next().next()
    }

    /// Whether width and height trade places.
    const fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Cw90 | Rotation::Ccw90)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Rotation::None => "as the firmware has it",
            Rotation::Cw90 => "quarter turn clockwise",
            Rotation::Half => "upside down",
            Rotation::Ccw90 => "quarter turn anticlockwise",
        }
    }
}

/// Where each colour channel sits in a framebuffer word.
///
/// Kept as shift-and-width rather than a match on [`PixelFormat`] so that
/// the bitmask formats — which are rare, but which some real firmware does
/// report — go through exactly the same arithmetic as the common ones.
#[derive(Clone, Copy)]
struct Channel {
    shift: u32,
    bits: u32,
}

impl Channel {
    const fn fixed(shift: u32) -> Self {
        Channel { shift, bits: 8 }
    }

    /// A channel described by a bitmask, which firmware is free to make any
    /// width it likes — 5 bits of red in a 16-bit word is legal, and so is
    /// a channel the hardware simply does not have.
    fn from_mask(mask: u32) -> Self {
        Channel { shift: mask.trailing_zeros(), bits: mask.count_ones().min(8) }
    }

    /// Place one 8-bit component, narrowing it if the channel is smaller.
    const fn pack(self, value: u8) -> u32 {
        if self.bits == 0 {
            return 0;
        }
        ((value >> (8 - self.bits)) as u32) << self.shift
    }
}

/// The firmware's framebuffer, plus the rotation applied on every write.
pub struct Framebuffer {
    base: *mut u32,
    /// Pixels per scan line, which is not always the width.
    stride: usize,
    /// Physical extent, in the firmware's own coordinates.
    width: usize,
    height: usize,
    red: Channel,
    green: Channel,
    blue: Channel,
    rotation: Rotation,
    /// Held only so the protocol stays open for the life of the image; the
    /// framebuffer address below would otherwise outlive our claim on it.
    _gop: uefi::boot::ScopedProtocol<GraphicsOutput>,
}

impl Framebuffer {
    /// Take the first usable graphics device, or `None` if there is none.
    ///
    /// Returning `None` is a normal outcome, not a failure: plenty of
    /// firmware offers only a text console, and the caller falls back to it.
    pub fn open() -> Option<Self> {
        let handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
        // GetProtocol, not an exclusive open, for the same reason the rest
        // of the application uses it: an exclusive claim disconnects the
        // drivers already consuming the interface, and the firmware's own
        // text console is one of them. Losing it would leave nothing to
        // fall back to if anything below this line goes wrong.
        //
        // SAFETY: GetProtocol neither installs nor removes interfaces, and
        // the ScopedProtocol is kept alive in the returned value.
        let mut gop = unsafe {
            uefi::boot::open_protocol::<GraphicsOutput>(
                uefi::boot::OpenProtocolParams {
                    handle,
                    agent: uefi::boot::image_handle(),
                    controller: None,
                },
                uefi::boot::OpenProtocolAttributes::GetProtocol,
            )
        }
        .ok()?;

        let info = gop.current_mode_info();
        // The mode the firmware picked is left alone. It is the one the
        // panel is actually driving, and on a fixed internal display,
        // changing it is a good way to end up looking at nothing.
        let (width, height) = info.resolution();
        let stride = info.stride();

        // BltOnly means there is no CPU-addressable framebuffer, so there
        // is nothing here to draw into.
        let (red, green, blue) = match info.pixel_format() {
            PixelFormat::Rgb => (Channel::fixed(0), Channel::fixed(8), Channel::fixed(16)),
            PixelFormat::Bgr => (Channel::fixed(16), Channel::fixed(8), Channel::fixed(0)),
            PixelFormat::Bitmask => {
                let mask = info.pixel_bitmask()?;
                // The masks also settle how wide a pixel is, and a bitmask
                // mode is free to be 16 bits — "5 bits of red in a 16-bit
                // word" below is legal. Everything under this module
                // indexes the framebuffer as 32-bit words, so any
                // narrower pixel would put every write at the wrong
                // offset and run half of `fill` past the framebuffer.
                // Rather than grow a second code path for hardware nobody
                // has seen this tool on, fall back to the text console.
                let union = mask.red | mask.green | mask.blue | mask.reserved;
                if (32 - union.leading_zeros()) < 25 {
                    return None;
                }
                (
                    Channel::from_mask(mask.red),
                    Channel::from_mask(mask.green),
                    Channel::from_mask(mask.blue),
                )
            }
            PixelFormat::BltOnly => return None,
        };
        if width == 0 || height == 0 || stride < width {
            return None;
        }

        let mut fb = gop.frame_buffer();
        // The SAFETY comments on `put` and `fill` assume the framebuffer
        // holds at least `stride * height` 32-bit pixels. That is what the
        // formats accepted above imply, but it is the firmware's buffer
        // and this is the one moment its actual size is in hand — so
        // check, rather than write past the end of a buffer some firmware
        // sized differently.
        let needed = stride.checked_mul(height)?.checked_mul(4)?;
        if fb.size() < needed {
            return None;
        }
        let base = fb.as_mut_ptr().cast::<u32>();
        Some(Framebuffer {
            base,
            stride,
            width,
            height,
            red,
            green,
            blue,
            rotation: Rotation::default(),
            _gop: gop,
        })
    }

    /// The orientation to start in.
    ///
    /// A framebuffer taller than it is wide is a panel mounted sideways —
    /// no desktop display is portrait by default, and no firmware chooses a
    /// portrait mode for a landscape screen. That is the Steam Deck, and
    /// the correction is a quarter turn clockwise. Anything else is taken
    /// at face value. The startup screen exists because this is a guess.
    pub const fn guess_rotation(&self) -> Rotation {
        if self.height > self.width {
            Rotation::Cw90
        } else {
            Rotation::None
        }
    }

    pub const fn rotation(&self) -> Rotation {
        self.rotation
    }

    pub const fn set_rotation(&mut self, rotation: Rotation) {
        self.rotation = rotation;
    }

    /// The drawable area in logical coordinates, which is the physical one
    /// with its axes swapped for the quarter turns.
    pub const fn logical_size(&self) -> (usize, usize) {
        if self.rotation.swaps_axes() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    pub const fn pack(&self, Rgb(r, g, b): Rgb) -> u32 {
        self.red.pack(r) | self.green.pack(g) | self.blue.pack(b)
    }

    /// Blend `fg` over `bg` at 8-bit coverage, already packed.
    pub const fn blend(&self, fg: Rgb, bg: Rgb, coverage: u8) -> u32 {
        const fn mix(fg: u8, bg: u8, a: u16) -> u8 {
            // +128 and the extra shift are the usual rounding trick; without
            // them light coverage rounds to nothing and thin strokes fade.
            let v = bg as u16 * (255 - a) + fg as u16 * a + 128;
            ((v + (v >> 8)) >> 8) as u8
        }
        let a = coverage as u16;
        self.pack(Rgb(mix(fg.0, bg.0, a), mix(fg.1, bg.1, a), mix(fg.2, bg.2, a)))
    }

    /// Logical to physical. Kept in one place so the four orientations can
    /// be checked against each other by eye.
    const fn map(&self, x: usize, y: usize) -> (usize, usize) {
        match self.rotation {
            Rotation::None => (x, y),
            Rotation::Cw90 => (self.width - 1 - y, x),
            Rotation::Half => (self.width - 1 - x, self.height - 1 - y),
            Rotation::Ccw90 => (y, self.height - 1 - x),
        }
    }

    /// Write one already-packed pixel at a logical position.
    ///
    /// Out-of-range coordinates are dropped rather than clamped: a clamp
    /// would smear a stray glyph along an edge, where a drop is invisible.
    fn put(&self, x: usize, y: usize, packed: u32) {
        let (w, h) = self.logical_size();
        if x >= w || y >= h {
            return;
        }
        let (px, py) = self.map(x, y);
        // SAFETY: `map` is a bijection onto the physical extent, and the
        // bounds check above puts (px, py) inside it. The framebuffer is at
        // least `stride * height` pixels, and the mode is never changed
        // after `open`, so `base` stays valid for the life of the image.
        unsafe { self.base.add(py * self.stride + px).write_volatile(packed) }
    }

    /// Paint the whole physical framebuffer one colour.
    ///
    /// Deliberately ignores rotation and works in scan lines: the result is
    /// identical whichever way round the picture is, and this is the one
    /// operation that touches every pixel, so it is worth not sending
    /// through [`Framebuffer::map`] a million times.
    pub fn fill(&self, color: Rgb) {
        let packed = self.pack(color);
        for row in 0..self.height {
            for col in 0..self.width {
                // SAFETY: as `put`, with the bound checked by the loops.
                unsafe { self.base.add(row * self.stride + col).write_volatile(packed) }
            }
        }
    }

    /// Fill a logical rectangle.
    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        let packed = self.pack(color);
        for dy in 0..h {
            for dx in 0..w {
                self.put(x + dx, y + dy, packed);
            }
        }
    }

    /// Draw one glyph over a filled cell.
    ///
    /// The cell is painted whole every time rather than only where the
    /// glyph changed, because that is what makes a repaint idempotent: the
    /// caller can redraw any cell at any moment without knowing what used
    /// to be there.
    pub fn draw_cell(&self, font: &font::Font, ch: char, x: usize, y: usize, fg: Rgb, bg: Rgb) {
        self.fill_rect(x, y, font.cell_w, font.cell_h, bg);

        let glyph = font.glyph(ch);
        if glyph.is_blank() {
            return;
        }
        for row in 0..usize::from(glyph.h) {
            for (col, &coverage) in font.row(glyph, row).iter().enumerate() {
                if coverage == 0 {
                    continue;
                }
                let packed =
                    if coverage == u8::MAX { self.pack(fg) } else { self.blend(fg, bg, coverage) };
                self.put(x + usize::from(glyph.x) + col, y + usize::from(glyph.y) + row, packed);
            }
        }
    }
}
