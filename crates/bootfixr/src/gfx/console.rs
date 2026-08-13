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
use super::font_data::MEDIUM;
#[cfg(not(feature = "tiny"))]
use super::font_data::{LARGE, SMALL};
use super::{Framebuffer, ModeChange, Rgb, Rotation};
use uefi::proto::console::text::Color;

/// The sixteen console colours, as this renderer draws them.
///
/// Close to the Tango palette, which was designed for exactly this — solid
/// colours that stay distinguishable without being garish. Three departures,
/// all for the panel this runs on: `DarkGray` is lifted well above Tango's,
/// because it carries `Style::Dim` and Tango's is barely visible on a bright
/// handheld screen; `LightRed` is softened, because a saturated red on black
/// fringes badly at this pixel density; and `LightMagenta` is brightened a
/// long way past Tango's plum, because it is the one colour that names a
/// button in the key hints and a muted mauve down there is what made those
/// hints easy to miss in the first place.
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
        Color::LightMagenta => Rgb(0xe0, 0x8f, 0xe8),
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
/// `bootfixr-tiny.efi` builds with the `tiny` feature, which keeps only
/// 12x24: enough to lay out the menus on every screen this runs on, and the
/// one size that has to stay whichever binary is on the ESP, so it is the
/// size worth keeping alone. With one entry, `resize_text` simply never
/// finds a next candidate and `automatic` always picks it — both fall out
/// of the existing logic rather than needing a separate path.
#[cfg(not(feature = "tiny"))]
static FONTS: [&Font; 3] = [&LARGE, &MEDIUM, &SMALL];
#[cfg(feature = "tiny")]
static FONTS: [&Font; 1] = [&MEDIUM];

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
/// instead lands on 12x24 and 106x33 on a Deck, drops to 8x16 only when the
/// screen genuinely cannot carry more, and steps up to 16x32 only on a
/// display large enough that 12x24 undershoots the target.
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

/// A grid and the cell size that produced it, for the display screen.
#[derive(Clone, Copy)]
pub struct Layout {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
}

/// What a framebuffer of this logical size would be laid out as, if it were
/// the one in use.
///
/// The display screen's answer to "what would that mode be worth?", and it
/// has to be [`automatic`]'s answer rather than a guess of its own, or the
/// number shown would not be the number a mode change delivered.
pub fn layout_for(width: usize, height: usize) -> Layout {
    let font = FONTS[automatic(width, height)];
    Layout {
        cols: width / font.cell_w,
        rows: height / font.cell_h,
        cell_w: font.cell_w,
        cell_h: font.cell_h,
    }
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

    /// Forget what is believed to be on the screen, leaving what is meant
    /// to be there alone.
    ///
    /// [`Console::relayout`] is the fuller version of this and rebuilds the
    /// grid; this one is for the case where the grid is still right and only
    /// the glass has been cleared underneath it.
    fn forget(&mut self) {
        self.shown.iter_mut().for_each(|c| *c = None);
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

    /// The grid as it stands, which is not always [`layout_for`]'s answer
    /// for this framebuffer: the operator may have stepped the cell size
    /// away from the automatic choice, and the display screen has to show
    /// what is on the glass rather than what would have been chosen.
    pub fn layout(&self) -> Layout {
        let font = self.font();
        Layout { cols: self.cols, rows: self.rows, cell_w: font.cell_w, cell_h: font.cell_h }
    }

    /// The modes the device offers, and the extent of the one in use.
    pub fn modes(&self) -> Vec<super::Mode> {
        self.fb.modes()
    }

    pub const fn resolution(&self) -> (usize, usize) {
        self.fb.resolution()
    }

    /// What a mode at `(width, height)` would come to if it were set — the
    /// grid [`resolution_menu`](crate::ui::resolution_menu) shows next to
    /// each mode it offers, so a choice can be made before it is risked.
    ///
    /// Turned before it is laid out, because the value of a mode depends on
    /// which way round the picture goes: 2560x1600 is a wide grid the way
    /// the firmware has it and a tall one on a panel mounted sideways.
    pub fn layout_at(&self, width: usize, height: usize) -> Layout {
        let (width, height) = self.fb.rotation().logical(width, height);
        layout_for(width, height)
    }

    /// Change the framebuffer mode and lay the grid out again in it.
    ///
    /// The cell size is chosen afresh rather than carried over: getting a
    /// bigger one is the reason to be here, and [`Console::relayout`] only
    /// asks again when the cell it has stopped fitting — which, on a step up
    /// to a larger framebuffer, is exactly when it has not.
    pub fn set_mode(&mut self, width: usize, height: usize) -> bool {
        match self.fb.set_mode(width, height) {
            ModeChange::Set => {}
            ModeChange::Refused => return false,
            // The firmware set a mode, could not be adopted, and the old
            // one was put back — two `SetMode` calls, each of which clears
            // the display. The grid still stands, because the geometry is
            // where it started, but nothing is on the glass and [`flush`]
            // draws only what differs from `shown`. Forget the screen so
            // that whatever is drawn next is drawn whole.
            ModeChange::Restored => {
                self.forget();
                return false;
            }
        }
        let (width, height) = self.fb.logical_size();
        self.font = automatic(width, height);
        self.relayout();
        true
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
