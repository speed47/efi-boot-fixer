//! The one place that knows which screen is being drawn on.
//!
//! Two backends sit behind this: the firmware's own
//! `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`, and [`crate::gfx`], which takes the
//! framebuffer and draws characters itself so the picture can be rotated to
//! match the panel. They are chosen once, at startup, and everything above
//! this module — every menu, every report, every confirmation — is written
//! against the primitives here and cannot tell the difference.
//!
//! That symmetry is the point. The graphical backend exists because the
//! Steam Deck's display is mounted sideways, not because the menus wanted
//! redesigning, and the tool has to keep working on firmware that offers no
//! usable framebuffer at all. Two renderings of one set of screens is the
//! only version of that which stays honest.

use core::cell::UnsafeCell;
use core::fmt::Write;

use crate::gfx::console::{rgb, Console};
use crate::gfx::{Framebuffer, Rotation};
use uefi::proto::console::text::Color;
use uefi::system;

// Exactly one of these exists, in the static below, for as long as the
// program runs. Boxing the large variant to even the two out would buy an
// allocation and a pointer hop on every character drawn, and save nothing.
#[allow(clippy::large_enum_variant)]
enum Backend {
    /// The firmware's text console, in whatever orientation it likes.
    Text,
    /// Our own, on the framebuffer.
    Gfx(Console),
}

/// Single-threaded interior mutability.
///
/// A UEFI application owns the machine: there is no scheduler, no other
/// thread, and interrupts do not run Rust code. The `uefi` crate's own
/// console handling rests on the same fact.
struct Slot(UnsafeCell<Option<Backend>>);

// SAFETY: as above — nothing else can reach this.
unsafe impl Sync for Slot {}

static BACKEND: Slot = Slot(UnsafeCell::new(None));

fn with<R>(f: impl FnOnce(&mut Backend) -> R) -> R {
    // SAFETY: single-threaded, and no primitive below re-enters `with`.
    let slot = unsafe { &mut *BACKEND.0.get() };
    f(slot.get_or_insert(Backend::Text))
}

/// Pick a backend, and take the orientation it guesses. Call once, before
/// anything is drawn.
///
/// The guess stands unless the operator says otherwise, which they do from
/// the display screen; [`rotation`] is how the rest of the module tells
/// whether there is one to argue with at all.
pub fn init() {
    let Some(mut fb) = Framebuffer::open() else {
        return;
    };
    let guess = fb.guess_rotation();
    fb.set_rotation(guess);

    // SAFETY: as `with`. Called before any drawing, so nothing is lost by
    // replacing whatever is in the slot.
    let slot = unsafe { &mut *BACKEND.0.get() };
    *slot = Some(Backend::Gfx(Console::new(fb)));
}

/// The current orientation, or `None` when the text console is in use and
/// the question does not arise.
pub fn rotation() -> Option<Rotation> {
    with(|b| match b {
        Backend::Gfx(console) => Some(console.rotation()),
        Backend::Text => None,
    })
}

pub fn set_rotation(rotation: Rotation) {
    with(|b| {
        if let Backend::Gfx(console) = b {
            console.set_rotation(rotation);
        }
    });
}

/// Step the cell size one up or down, reporting whether it moved.
///
/// Nothing to do on the text console: its mode is the firmware's business
/// and there is no font of ours involved.
pub fn resize_text(bigger: bool) -> bool {
    with(|b| match b {
        Backend::Gfx(console) => console.resize_text(bigger),
        Backend::Text => false,
    })
}

pub fn size() -> (usize, usize) {
    with(|b| match b {
        Backend::Gfx(console) => console.size(),
        Backend::Text => system::with_stdout(|out| {
            out.current_mode().ok().flatten().map(|m| (m.columns(), m.rows())).unwrap_or((80, 25))
        }),
    })
}

pub fn clear() {
    with(|b| match b {
        Backend::Gfx(console) => console.clear(),
        Backend::Text => system::with_stdout(|out| {
            let _ = out.clear();
        }),
    });
}

pub fn at(column: usize, row: usize) {
    with(|b| match b {
        Backend::Gfx(console) => console.set_cursor(column, row),
        Backend::Text => system::with_stdout(|out| {
            let _ = out.set_cursor_position(column, row);
        }),
    });
}

pub fn set_color(fg: Color, bg: Color) {
    with(|b| match b {
        Backend::Gfx(console) => console.set_color(rgb(fg), rgb(bg)),
        Backend::Text => system::with_stdout(|out| {
            let _ = out.set_color(fg, bg);
        }),
    });
}

pub fn write(text: &str) {
    with(|b| match b {
        Backend::Gfx(console) => console.write(text),
        Backend::Text => system::with_stdout(|out| {
            let _ = out.write_str(text);
        }),
    });
}

/// Put the drawn screen in front of the operator.
///
/// A no-op on the text console, which has already done it. The graphical
/// backend batches instead, so that a repaint costs only the cells that
/// changed; this is where that batch is spent. Called immediately before
/// blocking for input, which is the only moment the screen has to be right.
pub fn flush() {
    with(|b| {
        if let Backend::Gfx(console) = b {
            console.flush();
        }
    });
}

/// Not conditional on the backend.
///
/// Ours never draws a cursor, but the firmware's console is still there and
/// still owns the same framebuffer we are drawing on. Whatever it has left
/// on screen is ours to clean up.
pub fn hide_cursor() {
    system::with_stdout(|out| {
        let _ = out.enable_cursor(false);
    });
}

/// Give the screen back on the way out.
///
/// The graphical backend has spent the whole run drawing on a framebuffer
/// the firmware's console also owns, in a different orientation, and the
/// firmware has no idea. Asking it to clear resets both halves of that —
/// the pixels, and the cursor position it believes it is at — so whatever
/// runs next starts on a clean screen instead of printing sideways across
/// our last menu.
pub fn hand_back() {
    system::with_stdout(|out| {
        let _ = out.set_color(Color::LightGray, Color::Black);
        let _ = out.clear();
        let _ = out.enable_cursor(true);
    });
}

/// Adapter so the screens can keep using `write!`-style formatting.
pub struct Out;

impl Write for Out {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        write(text);
        Ok(())
    }
}

macro_rules! out {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::ui::term::Out, $($arg)*);
    }};
}

macro_rules! outln {
    () => { $crate::ui::term::write("\n") };
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::ui::term::Out, $($arg)*);
    }};
}

pub(crate) use {out, outln};
