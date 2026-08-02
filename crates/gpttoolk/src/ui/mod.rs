//! Screen handling and the operator's side of the conversation.
//!
//! Built for a Steam Deck in firmware, which means: no keyboard, no
//! scrollback, and the measured input set from `docs/efiprobe-deck.log` —
//! a D-pad, A (CR), B (ESCAPE), View (TAB), and a relative pointer. Every
//! screen therefore has to fit the display and be navigable with a D-pad.
//!
//! The screens are drawn against a character grid with cursor positioning
//! and sixteen colours, so these are real full-screen menus with a
//! highlight bar and nesting, not a scrolling transcript. That grid is
//! provided by one of two backends, chosen at startup by [`term`]: the
//! firmware's own text console, or [`crate::gfx`] drawing on the
//! framebuffer. The second exists so the picture can be turned to match a
//! panel that is mounted sideways, which the Steam Deck's is; nothing in
//! this module knows which one it is talking to.
//!
//! Report *formatting* lives in `gptcore::report`, which has no UEFI
//! dependency and is tested on the host. This module only paints lines and
//! reads answers back. Which colour a line gets is decided by the
//! [`Style`] the writer attached to it, never by inspecting the text here:
//! rewording a message must not silently change what it looks like.

pub(crate) mod term;

use crate::gfx::Rotation;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use gptcore::style::{Line, Style};
use term::{out, outln};
use uefi::boot;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::system;

/// What the hardware can actually say.
///
/// Note two collisions measured on the Deck: QAM is indistinguishable from
/// A, and the burger button from B. Neither can carry a separate meaning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    Up,
    Down,
    Left,
    Right,
    /// A, or QAM.
    Select,
    /// B, or the burger button.
    Cancel,
    /// The View button.
    Tab,
}

fn poll() -> Option<Input> {
    let key = system::with_stdin(|stdin| stdin.read_key().ok().flatten())?;
    match key {
        Key::Special(ScanCode::UP) => Some(Input::Up),
        Key::Special(ScanCode::DOWN) => Some(Input::Down),
        Key::Special(ScanCode::LEFT) => Some(Input::Left),
        Key::Special(ScanCode::RIGHT) => Some(Input::Right),
        Key::Special(ScanCode::ESCAPE) => Some(Input::Cancel),
        Key::Printable(c) => match char::from(c) {
            '\r' | '\n' => Some(Input::Select),
            '\t' => Some(Input::Tab),
            _ => None,
        },
        _ => None,
    }
}

/// How long each idle turn of an input loop sleeps.
const TICK_US: usize = 10_000;

/// Block until the operator does something.
///
/// Also the moment the drawn screen is put up: everything above paints into
/// a buffer, and there is no point showing a half-built screen, so the
/// flush happens here, once, immediately before there is anything to wait
/// for. Screens that never wait never needed to be seen.
pub fn wait() -> Input {
    term::flush();
    loop {
        if let Some(i) = poll() {
            return i;
        }
        boot::stall(TICK_US);
    }
}

/// Wait, but give up after roughly `millis`.
fn wait_for(millis: usize) -> Option<Input> {
    term::flush();
    let mut waited = 0;
    while waited < millis * 1000 {
        if let Some(i) = poll() {
            return Some(i);
        }
        boot::stall(TICK_US);
        waited += TICK_US;
    }
    None
}

/// Discard anything already queued.
///
/// Keys auto-repeat on this hardware (~10/s while held) and the firmware
/// buffers them, so a burst can outlive the screen that provoked it. Any
/// screen that asks a question worth getting right drains first.
pub fn drain() {
    term::flush();
    while poll().is_some() {}
    // A held button keeps producing events after the buffer empties; give
    // it a moment and sweep again.
    boot::stall(150_000);
    while poll().is_some() {}
}

pub fn size() -> (usize, usize) {
    term::size()
}

// ------------------------------------------------------------------ colour

/// Body text. Also the state everything is restored to, so a screen never
/// inherits a colour from whatever drew before it.
const BODY: (Color, Color) = (Color::LightGray, Color::Black);
/// The selected row in a menu.
const HIGHLIGHT: (Color, Color) = (Color::Black, Color::Cyan);

/// Semantic style to a concrete pair.
///
/// The light variants are used throughout because a UEFI console renders
/// the dark ones very dark on a black background, and this has to be
/// legible on a 7-inch handheld display at arm's length.
fn colors(style: Style) -> (Color, Color) {
    match style {
        Style::Normal => BODY,
        Style::Title => (Color::White, Color::Black),
        Style::Dim => (Color::DarkGray, Color::Black),
        Style::Good => (Color::LightGreen, Color::Black),
        Style::Warn => (Color::Yellow, Color::Black),
        Style::Bad => (Color::LightRed, Color::Black),
        Style::Key => (Color::LightCyan, Color::Black),
    }
}

fn paint(colors: (Color, Color)) {
    term::set_color(colors.0, colors.1);
}

fn body() {
    paint(BODY);
}

pub fn clear() {
    // Reset first: `clear` fills the screen with the *current* background,
    // so clearing while a highlight is active paints the whole display.
    body();
    term::clear();
}

fn at(column: usize, row: usize) {
    term::at(column, row);
}

pub fn hide_cursor() {
    term::hide_cursor();
}

/// Hand the screen back to the firmware. Call once, on the way out.
pub fn finish() {
    term::hand_back();
}

/// Clip to the display width.
///
/// A device path is routinely longer than 80 columns, and letting it wrap
/// pushes everything below it off the bottom of a screen that cannot
/// scroll back.
/// The widest a line may be before [`fit`] starts cutting it.
///
/// One short of the screen, so a full-width line does not wrap the cursor
/// onto the next row of its own accord. [`wrapped`] targets the same number
/// — a wrap that produced lines `fit` then truncated would be worse than
/// useless.
fn text_width(columns: usize) -> usize {
    columns.saturating_sub(1).max(8)
}

/// Break a long value across lines instead of cutting it off with a `~`.
///
/// The screen width is fixed once the menus start — the only place that
/// changes the cell size is the startup Display screen — so callers may
/// wrap when they build their lines rather than on every repaint.
pub fn wrapped(text: &str, style: Style, hang: &str) -> Vec<Line> {
    gptcore::style::wrap(text, text_width(size().0), style, hang)
}

fn fit(line: &str, columns: usize) -> String {
    let limit = text_width(columns);
    if line.chars().count() <= limit {
        return line.to_string();
    }
    let mut out: String = line.chars().take(limit.saturating_sub(1)).collect();
    out.push('~');
    out
}

/// Print one styled line, then return to body colour.
fn styled(line: &Line, columns: usize) {
    if line.style == Style::Normal {
        outln!("{}", fit(&line.text, columns));
        return;
    }
    paint(colors(line.style));
    outln!("{}", fit(&line.text, columns));
    body();
}

/// The line under a heading, spanning the writable width.
///
/// One short of the full width, so that writing it never puts the cursor
/// past the last column and costs a wrapped blank line.
fn rule(columns: usize) -> String {
    "-".repeat(columns.saturating_sub(1))
}

fn header(title: &str) {
    let (cols, _) = size();
    at(0, 0);
    paint(colors(Style::Title));
    outln!("{}", fit(title, cols));
    paint(colors(Style::Dim));
    outln!("{}", rule(cols));
    body();
}

/// The key hints along the bottom, which are chrome and should not compete
/// with the content above them.
fn footer(rows: usize, text: &str) {
    at(0, rows.saturating_sub(2));
    paint(colors(Style::Dim));
    out!("{text}");
    body();
}

// -------------------------------------------------------------------- menu

/// One selectable line, plus whatever should be shown about it while it is
/// selected.
pub struct Item {
    pub label: String,
    pub detail: Vec<Line>,
}

impl Item {
    pub fn with_detail(label: impl Into<String>, detail: Vec<Line>) -> Self {
        Item { label: label.into(), detail }
    }
}

/// Lines moved by one press of up or down in a report.
///
/// Not one. The Deck's D-pad repeats at about 1.8/s while held — measured,
/// in `docs/efiprobe-deck.log` — so a line at a time makes a long report a
/// chore to walk. Three is enough to make progress without losing your
/// place, and left and right still move a whole screen for getting
/// somewhere else entirely.
const SCROLL_LINES: usize = 3;

/// A scrollable page of text, ending in "continue" or "cancel".
///
/// Returns false if the operator backed out. A Deck cannot scroll back, so
/// anything longer than the screen has to be navigable rather than simply
/// printed and lost.
pub fn page(title: &str, lines: &[Line]) -> bool {
    let (cols, rows) = size();
    let view = rows.saturating_sub(6).max(4);
    let mut top = 0usize;
    let max_top = lines.len().saturating_sub(view);
    drain();

    loop {
        clear();
        header(title);
        for line in lines.iter().skip(top).take(view) {
            styled(line, cols);
        }
        if lines.len() > view {
            at(0, rows.saturating_sub(3));
            paint(colors(Style::Key));
            outln!(
                "  lines {}-{} of {}   up/down = {} lines, left/right = a screen",
                top + 1,
                (top + view).min(lines.len()),
                lines.len(),
                SCROLL_LINES
            );
            body();
        }
        footer(rows, "  A = continue    B = back");

        match wait() {
            Input::Up => top = top.saturating_sub(SCROLL_LINES),
            Input::Down => top = (top + SCROLL_LINES).min(max_top),
            Input::Left => top = top.saturating_sub(view),
            Input::Right => top = (top + view).min(max_top),
            Input::Select => return true,
            Input::Cancel => return false,
            Input::Tab => {}
        }
    }
}

/// A message with a single acknowledgement.
pub fn message(title: &str, lines: &[Line]) {
    let (cols, rows) = size();
    clear();
    header(title);
    for line in lines {
        styled(line, cols);
    }
    footer(rows, "  A = continue");
    drain();
    loop {
        match wait() {
            Input::Select | Input::Cancel => return,
            _ => {}
        }
    }
}

/// What came back from a menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Item(usize),
    /// View was pressed on this item: show it in full, then come back.
    Inspect(usize),
    Cancelled,
}

/// A D-pad menu with a highlight bar. Returns the chosen index, or `None`
/// if the operator backed out.
///
/// `hint` names what B does here, which differs between the top level
/// ("exit") and a submenu ("back").
pub fn menu(title: &str, intro: &[Line], items: &[Item], hint: &str) -> Option<usize> {
    match run_menu(title, intro, items, hint, None, 0) {
        Choice::Item(i) => Some(i),
        _ => None,
    }
}

/// A menu whose entries can also be opened for a closer look.
///
/// `start` is the initially selected row, so returning from an inspection
/// puts the operator back where they were rather than at the top.
pub fn menu_inspectable(
    title: &str,
    intro: &[Line],
    items: &[Item],
    hint: &str,
    inspect_hint: &str,
    start: usize,
) -> Choice {
    run_menu(title, intro, items, hint, Some(inspect_hint), start)
}

fn run_menu(
    title: &str,
    intro: &[Line],
    items: &[Item],
    hint: &str,
    inspect_hint: Option<&str>,
    start: usize,
) -> Choice {
    if items.is_empty() {
        return Choice::Cancelled;
    }
    let mut selected = start.min(items.len() - 1);
    let mut top = 0usize;
    drain();

    loop {
        let (cols, rows) = size();
        let detail_rows = items.iter().map(|i| i.detail.len()).max().unwrap_or(0);
        // title, rule, intro, blank, [items], blank, detail, blank, hint
        let overhead = 2 + intro.len() + 1 + detail_rows + 3;
        let view = rows.saturating_sub(overhead).clamp(1, items.len());

        // Keep the selection inside the window.
        if selected < top {
            top = selected;
        } else if selected >= top + view {
            top = selected + 1 - view;
        }

        clear();
        header(title);
        for line in intro {
            styled(line, cols);
        }
        outln!();

        for (i, item) in items.iter().enumerate().skip(top).take(view) {
            if i == selected {
                paint(HIGHLIGHT);
                // Pad so the bar spans the row rather than hugging the text.
                let text = fit(&item.label, cols.saturating_sub(4));
                let width = cols.saturating_sub(5);
                outln!("  {text:<width$} ");
                body();
            } else {
                outln!("   {}", fit(&item.label, cols.saturating_sub(3)));
            }
        }
        if items.len() > view {
            paint(colors(Style::Dim));
            outln!("   ... {} of {}", selected + 1, items.len());
            body();
        }

        outln!();
        for line in &items[selected].detail {
            styled(line, cols);
        }

        let keys = match inspect_hint {
            Some(extra) => alloc::format!("  D-pad = move    A = choose    {extra}    {hint}"),
            None => alloc::format!("  D-pad = move    A = choose    {hint}"),
        };
        footer(rows, &keys);

        match wait() {
            Input::Up => {
                selected = if selected == 0 { items.len() - 1 } else { selected - 1 };
            }
            Input::Down => selected = (selected + 1) % items.len(),
            Input::Select => return Choice::Item(selected),
            Input::Cancel => return Choice::Cancelled,
            Input::Tab if inspect_hint.is_some() => return Choice::Inspect(selected),
            _ => {}
        }
    }
}

// ----------------------------------------------------------------- consent

/// The sequence that authorises a write.
///
/// Chosen over hold-to-confirm deliberately: it depends only on discrete
/// presses, so it does not rely on the firmware's auto-repeat behaviour,
/// and a buffered burst of repeats cannot walk through it. Five specific
/// presses in order is not something a thumb does by accident.
pub const CONFIRM_SEQUENCE: [Input; 5] =
    [Input::Left, Input::Right, Input::Left, Input::Right, Input::Select];

fn step_name(i: Input) -> &'static str {
    match i {
        Input::Up => "UP",
        Input::Down => "DOWN",
        Input::Left => "LEFT",
        Input::Right => "RIGHT",
        Input::Select => "A",
        Input::Cancel => "B",
        Input::Tab => "VIEW",
    }
}

/// Draw the progress boxes, colouring each one individually: done in
/// green, the one being waited for in cyan, the rest dim. Printed piece by
/// piece rather than as a formatted line, because this is the one place
/// where per-word colour genuinely helps.
fn draw_steps(position: usize) {
    let mut names = String::from("     ");
    for step in CONFIRM_SEQUENCE.iter() {
        names.push_str(step_name(*step));
        names.push_str("  ");
    }
    paint(colors(Style::Dim));
    outln!("{names}");
    body();

    out!("     ");
    for (i, step) in CONFIRM_SEQUENCE.iter().enumerate() {
        let (mark, style) = match i {
            _ if i < position => ("[x]", Style::Good),
            _ if i == position => ("[ ]", Style::Key),
            _ => ("[ ]", Style::Dim),
        };
        paint(colors(style));
        out!("{mark}");
        body();
        for _ in 0..step_name(*step).len().saturating_sub(1) {
            out!(" ");
        }
    }
    outln!();
}

/// Require [`CONFIRM_SEQUENCE`] before a destructive write.
///
/// Any wrong press resets progress. B cancels outright.
pub fn confirm_sequence(title: &str, warning: &[Line]) -> bool {
    let (cols, rows) = size();
    let mut position = 0usize;
    let mut wrong = false;
    drain();

    loop {
        clear();
        header(title);
        for line in warning {
            styled(line, cols);
        }
        outln!();
        paint(colors(Style::Title));
        outln!("  To authorise this, press in order:");
        body();
        outln!();

        draw_steps(position);

        outln!();
        if wrong {
            paint(colors(Style::Bad));
            outln!("  wrong button - sequence reset");
            body();
        } else if position > 0 {
            paint(colors(Style::Key));
            outln!("  next: {}", step_name(CONFIRM_SEQUENCE[position]));
            body();
        }

        footer(rows, "  B = cancel, nothing is written");

        let input = wait();
        if input == Input::Cancel {
            return false;
        }
        if input == CONFIRM_SEQUENCE[position] {
            wrong = false;
            position += 1;
            if position == CONFIRM_SEQUENCE.len() {
                return true;
            }
        } else {
            // Restarting from a correct first press is the common case
            // after a fumble; do not make them lift off and start again.
            wrong = true;
            position = usize::from(input == CONFIRM_SEQUENCE[0]);
        }
    }
}

// ----------------------------------------------------------------- display

/// How long the display screen waits before deciding it was right.
const DISPLAY_SECONDS: usize = 6;

/// Choose a backend, and settle which way up the screen goes.
///
/// Call once, before anything else draws.
pub fn init() {
    // Before taking the framebuffer, not after: the firmware's console is
    // about to have its screen drawn on, and anything it puts there on the
    // way out should land where our first fill will wipe it.
    hide_cursor();

    let Some(guess) = term::init() else {
        // No usable framebuffer. The firmware's console it is, in whatever
        // orientation the firmware has it — which is the situation this
        // tool shipped in, and still works.
        return;
    };
    if guess != Rotation::None {
        display();
    }
}

/// Offer to turn the picture, and to change how much of it fits.
///
/// Shown only when [`crate::gfx::Framebuffer::guess_rotation`] guessed, which
/// means a portrait framebuffer, which means a panel mounted sideways. The
/// guess is almost certainly right, so this continues on its own after a
/// few seconds rather than demanding a press every launch. The moment
/// anything is pressed it stops counting and waits properly: someone
/// pressing a button here is someone who cannot read the screen, and they
/// should not be racing a timer while they fix it.
///
/// LEFT and RIGHT work regardless of which way round the text came out,
/// which is the only reason this screen can rescue a wrong guess at all.
/// UP and DOWN step the text size, because how big is comfortable is a
/// judgement about a particular person looking at a particular panel from
/// wherever they happen to be holding it, and no amount of arithmetic here
/// settles it.
fn display() {
    let mut rotation = term::rotation().unwrap_or_default();
    let mut countdown = Some(DISPLAY_SECONDS);
    let mut at_limit = false;
    drain();

    loop {
        let (cols, rows) = size();
        clear();
        header("Display");
        outln!("  This screen is drawn by the toolkit itself, so that it can be");
        outln!("  turned to match the panel. This one is mounted sideways, and");
        outln!("  the firmware's own text comes out a quarter turn off.");
        outln!();
        paint(colors(Style::Key));
        outln!("  Now showing: {}, {cols} x {rows} characters", fit(rotation.name(), cols));
        body();
        outln!();
        outln!("  If you can read this the right way up, there is nothing to do.");

        if at_limit {
            paint(colors(Style::Dim));
            outln!("  No further text size that way.");
            body();
        } else {
            match countdown {
                Some(0) => return,
                Some(left) => {
                    paint(colors(Style::Dim));
                    outln!("  Continuing in {left}s.");
                    body();
                }
                None => {}
            }
        }

        footer(rows, "  LEFT / RIGHT = turn    UP / DOWN = text size    A = done");

        let input = match countdown {
            Some(left) => match wait_for(1000) {
                None => {
                    countdown = Some(left - 1);
                    continue;
                }
                // Any press at all stops the clock, including the one being
                // acted on below.
                Some(input) => {
                    countdown = None;
                    input
                }
            },
            None => wait(),
        };

        at_limit = false;
        match input {
            Input::Left => rotation = rotation.previous(),
            Input::Right => rotation = rotation.next(),
            Input::Up => at_limit = !term::resize_text(true),
            Input::Down => at_limit = !term::resize_text(false),
            Input::Select | Input::Cancel => return,
            Input::Tab => continue,
        }
        term::set_rotation(rotation);
    }
}
