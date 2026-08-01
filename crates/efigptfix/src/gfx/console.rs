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
use super::font_data::{LARGE, SMALL};
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

/// The narrowest and shortest grid worth using the large font for.
///
/// 80x25 is what the menus were laid out against, so the large font is
/// taken whenever it still reaches that. On a Steam Deck it does exactly:
/// 1280x800 of logical screen over a 16x32 cell is 80 by 25 with nothing
/// left over. Smaller framebuffers — OVMF's 800x600, most of all — drop to
/// the small font rather than lose columns.
const MIN_COLS: usize = 80;
const MIN_ROWS: usize = 25;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    /// ASCII only; everything drawn by this application is.
    ch: u8,
    fg: Rgb,
    bg: Rgb,
}

pub struct Console {
    fb: Framebuffer,
    font: &'static Font,
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
        let mut console = Console {
            fb,
            font: &SMALL,
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

    /// Recompute the grid for the current orientation and repaint from
    /// scratch. The only place that fills the whole framebuffer.
    fn relayout(&mut self) {
        let (width, height) = self.fb.logical_size();

        self.font = if width / LARGE.cell_w >= MIN_COLS && height / LARGE.cell_h >= MIN_ROWS {
            &LARGE
        } else {
            &SMALL
        };
        self.cols = width / self.font.cell_w;
        self.rows = height / self.font.cell_h;
        self.origin = (
            (width - self.cols * self.font.cell_w) / 2,
            (height - self.rows * self.font.cell_h) / 2,
        );

        let blank = Cell { ch: b' ', fg: self.fg, bg: self.bg };
        self.cells = vec![blank; self.cols * self.rows];
        self.shown = vec![None; self.cols * self.rows];
        self.cursor = (0, 0);
        self.fb.fill(self.bg);
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
        for row in 0..self.rows {
            for col in 0..self.cols {
                let index = row * self.cols + col;
                let cell = self.cells[index];
                if self.shown[index] == Some(cell) {
                    continue;
                }
                self.fb.draw_cell(
                    self.font,
                    char::from(cell.ch),
                    self.origin.0 + col * self.font.cell_w,
                    self.origin.1 + row * self.font.cell_h,
                    cell.fg,
                    cell.bg,
                );
                self.shown[index] = Some(cell);
            }
        }
    }
}
