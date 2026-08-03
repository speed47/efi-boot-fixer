//! The shape of the baked glyph data.
//!
//! Everything here describes what `tools/mkfont` writes into
//! [`font_data`](super::font_data); nothing decides how a glyph is coloured
//! or where it lands, which is the framebuffer's business.

/// One glyph, trimmed to the pixels it actually lights.
///
/// `x`/`y` put the box back where it belongs inside the cell. A blank glyph
/// — space, and anything the face had no outline for — has zero width.
pub struct Glyph {
    pub x: u8,
    pub y: u8,
    pub w: u8,
    pub h: u8,
    /// Where this glyph's rows start in the font's coverage pool.
    pub off: u32,
}

impl Glyph {
    pub const fn is_blank(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// A fixed-cell font: 8-bit coverage, one contiguous pool of trimmed boxes.
pub struct Font {
    pub cell_w: usize,
    pub cell_h: usize,
    /// Code point of `glyphs[0]`.
    pub first: u8,
    pub glyphs: &'static [Glyph],
    pub coverage: &'static [u8],
}

/// Stood in for anything outside the baked range. The application's output
/// is ASCII by construction, so this is a backstop, not a code path.
const MISSING: char = '?';

/// Marks a menu's selected row.
///
/// DEL (0x7F) is the one ASCII code point never emitted as text, so
/// `tools/mkfont` bakes an arrow (U+27A4) into that slot instead of leaving
/// it blank. Sending this through the console cell grid still fits: DEL is
/// 7-bit, same as everything else this application draws.
pub const ARROW: char = '\u{7f}';

static BLANK: Glyph = Glyph { x: 0, y: 0, w: 0, h: 0, off: 0 };

impl Font {
    pub fn glyph(&self, ch: char) -> &Glyph {
        self.lookup(ch).or_else(|| self.lookup(MISSING)).unwrap_or(&BLANK)
    }

    fn lookup(&self, ch: char) -> Option<&Glyph> {
        let code = u8::try_from(u32::from(ch)).ok()?;
        self.glyphs.get(usize::from(code.checked_sub(self.first)?))
    }

    /// One row of a glyph's coverage box, left to right.
    pub fn row(&self, glyph: &Glyph, row: usize) -> &[u8] {
        let width = usize::from(glyph.w);
        let start = glyph.off as usize + row * width;
        self.coverage.get(start..start + width).unwrap_or(&[])
    }
}
