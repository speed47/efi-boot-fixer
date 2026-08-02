//! A character console built on the framebuffer.
//!
//! Deliberately the same shape as `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`: a grid
//! of cells, a cursor, a current colour pair. That is what lets every menu
//! in `ui` be written once and drawn by either backend, and it means this
//! module can be judged on one question — does it put the same characters
//! in the same places as the firmware would?
//!
//! The one thing it does differently is when pixels move. [`Console::clear`]
//! does not touch the framebuffer; it blanks the grid, and
//! [`Console::flush`] later repaints only the cells whose contents actually
//! changed. The menus clear and redraw on every keypress, so a `clear` that
//! really filled the screen would show as a black flash under the cursor
//! each time the operator moved it. Diffing makes moving the highlight bar
//! cost two rows of repaint instead of a whole screen.

use alloc::vec;
use alloc::vec::Vec;

use super::font::Font;
use super::font_data::{MEDIUM, SMALL};
use super::{Framebuffer, Rgb, Rotation};
use uefi::proto::console::text::Color;

/// The sixteen console colours, as this renderer draws them.
///
/// Close to the Tango palette, which was designed for exactly this — solid
/// colours that stay distinguishable without being garish. Two departures,
/// both for the panel this runs on: `DarkGray` is lifted well above
/// Tango's, because it carries `Style::Dim` and Tango's is barely visible
/// on a bright handheld screen, and `LightRed` is softened, because a
/// saturated red on black fringes badly at this pixel density.
pub fn rgb(color: Color) -> Rgb {
    match color {
        Color::Black => Rgb(0x10, 0x12, 0x16),
        Color::Blue => Rgb(0x34, 0x65, 0xa4),
        Color::Green => Rgb(0x4e, 0x9a, 0x06),
        Color::Cyan => Rgb(0x06, 0x98, 0x9a),
        Color::Red => Rgb(0xcc, 0x00, 0x00),
        Color::Magenta => Rgb(0x75, 0x50, 0x7b),
        Color::Brown => Rgb(0xc4, 0xa0, 0x00),
        Color::LightGray => Rgb(0xd3, 0xd7, 0xcf),
        Color::DarkGray => Rgb(0x8a, 0x90, 0x99),
        Color::LightBlue => Rgb(0x72, 0x9f, 0xcf),
        Color::LightGreen => Rgb(0x8a, 0xe2, 0x34),
        Color::LightCyan => Rgb(0x34, 0xe2, 0xe2),
        Color::LightRed => Rgb(0xef, 0x6b, 0x6b),
        Color::LightMagenta => Rgb(0xad, 0x7f, 0xa8),
        Color::Yellow => Rgb(0xfc, 0xe9, 0x4f),
        Color::White => Rgb(0xff, 0xff, 0xff),
    }
}

/// The cell sizes available, largest first.
///
/// Largest first is load-bearing in two places: [`Console::resize_text`]
/// reads "bigger" as one step *down* the index, and [`automatic`] falls
/// back to the last entry when nothing clears the minimum, which has to be
/// the smallest to stand a chance.
///
/// `fits` is also monotone along the order, since a smaller cell can only
/// ever give more rows and columns. That is what lets `resize_text` treat
/// one candidate that does not fit as "nothing further that way" and stop,
/// rather than having to look past it for a smaller size that might. Note
/// that it does still check the candidate — monotonicity saves the scan,
/// not the test. [`automatic`] does not step at all: it scores every size
/// that fits against [`TARGET_COLS`] and takes the closest.
///
/// There was a 16x32 above these. Measured on a Steam Deck, 12x24 already
/// reads comfortably at arm's length on a 7-inch panel, so the larger cell
/// was only ever costing columns — and 22 KB of baked glyphs. With two
/// entries left, that scoring loop decides between exactly two candidates;
/// it stays written as a search because the sizes are a build-time choice
/// and this is the code that would have to be right if a third returned.
static FONTS: [&Font; 2] = [&MEDIUM, &SMALL];

/// The smallest grid the menus were laid out against. Nothing narrower or
/// shorter is offered, automatically or on request.
const MIN_COLS: usize = 80;
const MIN_ROWS: usize = 25;

/// The line length aimed for when choosing a cell size.
///
/// Picking the *largest* cell that clears the minimum was the obvious rule
/// and the wrong one: on any screen that cleared 80 columns by a little, it
/// chose the coarsest size that fitted and left long device paths truncated
/// and reports paginated that did not need to be. Aiming at a line length
/// instead lands on 12x24 and 106x33 on a Deck, and drops to 8x16 only when
/// the screen genuinely cannot carry more.
const TARGET_COLS: usize = 104;

/// Whether a cell size yields a grid the menus fit in.
fn fits(font: &Font, width: usize, height: usize) -> bool {
    width / font.cell_w >= MIN_COLS && height / font.cell_h >= MIN_ROWS
}

/// The cell size to start with: whichever lands nearest [`TARGET_COLS`],
/// among those that fit.
///
/// Falls back to the smallest, which is the only honest answer on a
/// framebuffer too small for any of them — the menus will be clipped, but
/// clipped is better than not drawn.
fn automatic(width: usize, height: usize) -> usize {
    let mut best = FONTS.len() - 1;
    let mut closest = usize::MAX;
    for (index, font) in FONTS.iter().enumerate() {
        if !fits(font, width, height) {
            continue;
        }
        let distance = (width / font.cell_w).abs_diff(TARGET_COLS);
        if distance < closest {
            closest = distance;
            best = index;
        }
    }
    best
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    /// ASCII only; everything drawn by this application is.
    ch: u8,
    fg: Rgb,
    bg: Rgb,
}

pub struct Console {
    fb: Framebuffer,
    /// Index into [`FONTS`]. Held as an index rather than a reference so
    /// the operator can step it up and down.
    font: usize,
    cols: usize,
    rows: usize,
    /// Logical pixel position of cell (0, 0). The grid rarely divides the
    /// screen exactly, and the remainder looks like a border rather than a
    /// mistake when it is split between both sides.
    origin: (usize, usize),
    /// What the next flush should put on the screen.
    cells: Vec<Cell>,
    /// What is believed to be on the screen. `None` means "unknown", which
    /// is the honest answer after a resize or a rotation.
    shown: Vec<Option<Cell>>,
    cursor: (usize, usize),
    fg: Rgb,
    bg: Rgb,
}

impl Console {
    pub fn new(fb: Framebuffer) -> Self {
        let (width, height) = fb.logical_size();
        let mut console = Console {
            fb,
            font: automatic(width, height),
            cols: 0,
            rows: 0,
            origin: (0, 0),
            cells: Vec::new(),
            shown: Vec::new(),
            cursor: (0, 0),
            fg: rgb(Color::LightGray),
            bg: rgb(Color::Black),
        };
        console.relayout();
        console
    }

    fn font(&self) -> &'static Font {
        FONTS[self.font]
    }

    /// Recompute the grid for the current cell size and orientation, and
    /// repaint from scratch. The only place that fills the whole
    /// framebuffer.
    fn relayout(&mut self) {
        let (width, height) = self.fb.logical_size();

        // A quarter turn swaps the axes, so a size chosen while the screen
        // was the other way round may no longer fit. Ask again rather than
        // hold on to a choice that has stopped making sense.
        if !fits(self.font(), width, height) {
            self.font = automatic(width, height);
        }
        let font = self.font();

        self.cols = width / font.cell_w;
        self.rows = height / font.cell_h;
        self.origin =
            ((width - self.cols * font.cell_w) / 2, (height - self.rows * font.cell_h) / 2);

        let blank = Cell { ch: b' ', fg: self.fg, bg: self.bg };
        self.cells = vec![blank; self.cols * self.rows];
        self.shown = vec![None; self.cols * self.rows];
        self.cursor = (0, 0);
        self.fb.fill(self.bg);
    }

    /// Step one cell size up or down, if there is one that still fits.
    ///
    /// Returns whether anything changed, so the caller can tell the
    /// operator they have run out of sizes rather than leaving them
    /// pressing a button that silently does nothing.
    pub fn resize_text(&mut self, bigger: bool) -> bool {
        let next = if bigger { self.font.checked_sub(1) } else { Some(self.font + 1) };
        let Some(next) = next.filter(|i| *i < FONTS.len()) else {
            return false;
        };
        let (width, height) = self.fb.logical_size();
        if !fits(FONTS[next], width, height) {
            return false;
        }
        self.font = next;
        self.relayout();
        true
    }

    pub const fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub const fn rotation(&self) -> Rotation {
        self.fb.rotation()
    }

    pub fn set_rotation(&mut self, rotation: Rotation) {
        if rotation != self.fb.rotation() {
            self.fb.set_rotation(rotation);
            self.relayout();
        }
    }

    pub const fn set_color(&mut self, fg: Rgb, bg: Rgb) {
        self.fg = fg;
        self.bg = bg;
    }

    /// Blank the grid. Pixels are left alone; see the module note.
    pub fn clear(&mut self) {
        let blank = Cell { ch: b' ', fg: self.fg, bg: self.bg };
        self.cells.iter_mut().for_each(|c| *c = blank);
        self.cursor = (0, 0);
    }

    pub const fn set_cursor(&mut self, col: usize, row: usize) {
        self.cursor = (col, row);
    }

    /// Write text at the cursor, wrapping at the right edge.
    ///
    /// Anything below the last row is dropped rather than scrolled. The
    /// firmware console would scroll, but nothing here wants it to: every
    /// screen is built to fit, and a screen that did overflow would be far
    /// better off losing its last line than silently shifting the rest of
    /// itself up past a heading the operator is reading.
    pub fn write(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => {
                    self.cursor.0 = 0;
                    self.cursor.1 += 1;
                }
                '\r' => self.cursor.0 = 0,
                _ => {
                    if self.cursor.0 >= self.cols {
                        self.cursor.0 = 0;
                        self.cursor.1 += 1;
                    }
                    if self.cursor.1 < self.rows {
                        let index = self.cursor.1 * self.cols + self.cursor.0;
                        let byte = u8::try_from(ch).unwrap_or(b'?');
                        self.cells[index] = Cell { ch: byte, fg: self.fg, bg: self.bg };
                    }
                    self.cursor.0 += 1;
                }
            }
        }
    }

    /// Draw every cell that changed since the last flush.
    pub fn flush(&mut self) {
        let font = self.font();
        for row in 0..self.rows {
            for col in 0..self.cols {
                let index = row * self.cols + col;
                let cell = self.cells[index];
                if self.shown[index] == Some(cell) {
                    continue;
                }
                self.fb.draw_cell(
                    font,
                    char::from(cell.ch),
                    self.origin.0 + col * font.cell_w,
                    self.origin.1 + row * font.cell_h,
                    cell.fg,
                    cell.bg,
                );
                self.shown[index] = Some(cell);
            }
        }
    }
}
