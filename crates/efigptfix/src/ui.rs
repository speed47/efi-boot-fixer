//! Screen handling and the operator's side of the conversation.
//!
//! Built for a Steam Deck in firmware, which means: no keyboard, no
//! scrollback, and the measured input set from `docs/efiprobe-deck.log` —
//! a D-pad, A (CR), B (ESCAPE), View (TAB), and a relative pointer. Every
//! screen therefore has to fit the display and be navigable with a D-pad.
//!
//! `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL` gives cursor positioning and sixteen
//! colours, so these are real full-screen menus with a highlight bar and
//! nesting, not a scrolling transcript.
//!
//! Report *formatting* lives in `gptcore::report`, which has no UEFI
//! dependency and is tested on the host. This module only paints lines and
//! reads answers back. Which colour a line gets is decided by the
//! [`Style`] the writer attached to it, never by inspecting the text here:
//! rewording a message must not silently change what it looks like.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use gptcore::style::{Line, Style};
use uefi::boot;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::{print, println, system};

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

/// Block until the operator does something.
pub fn wait() -> Input {
    loop {
        if let Some(i) = poll() {
            return i;
        }
        boot::stall(10_000);
    }
}

/// Discard anything already queued.
///
/// Keys auto-repeat on this hardware (~10/s while held) and the firmware
/// buffers them, so a burst can outlive the screen that provoked it. Any
/// screen that asks a question worth getting right drains first.
pub fn drain() {
    while poll().is_some() {}
    // A held button keeps producing events after the buffer empties; give
    // it a moment and sweep again.
    boot::stall(150_000);
    while poll().is_some() {}
}

pub fn size() -> (usize, usize) {
    system::with_stdout(|out| {
        out.current_mode().ok().flatten().map(|m| (m.columns(), m.rows())).unwrap_or((80, 25))
    })
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
    system::with_stdout(|out| {
        let _ = out.set_color(colors.0, colors.1);
    });
}

fn body() {
    paint(BODY);
}

pub fn clear() {
    // Reset first: `clear` fills the screen with the *current* background,
    // so clearing while a highlight is active paints the whole display.
    body();
    system::with_stdout(|out| {
        let _ = out.clear();
    });
}

fn at(column: usize, row: usize) {
    system::with_stdout(|out| {
        let _ = out.set_cursor_position(column, row);
    });
}

pub fn hide_cursor() {
    system::with_stdout(|out| {
        let _ = out.enable_cursor(false);
    });
}

/// Clip to the display width.
///
/// A device path is routinely longer than 80 columns, and letting it wrap
/// pushes everything below it off the bottom of a screen that cannot
/// scroll back.
fn fit(line: &str, columns: usize) -> String {
    let limit = columns.saturating_sub(1).max(8);
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
        println!("{}", fit(&line.text, columns));
        return;
    }
    paint(colors(line.style));
    println!("{}", fit(&line.text, columns));
    body();
}

fn rule(columns: usize) -> String {
    "-".repeat(columns.saturating_sub(1).min(78))
}

fn header(title: &str) {
    let (cols, _) = size();
    at(0, 0);
    paint(colors(Style::Title));
    println!("{}", fit(title, cols));
    paint(colors(Style::Dim));
    println!("{}", rule(cols));
    body();
}

/// The key hints along the bottom, which are chrome and should not compete
/// with the content above them.
fn footer(rows: usize, text: &str) {
    at(0, rows.saturating_sub(2));
    paint(colors(Style::Dim));
    print!("{text}");
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
            println!(
                "  lines {}-{} of {}   D-pad up/down to scroll",
                top + 1,
                (top + view).min(lines.len()),
                lines.len()
            );
            body();
        }
        footer(rows, "  A = continue    B = back");

        match wait() {
            Input::Up => top = top.saturating_sub(1),
            Input::Down => top = (top + 1).min(max_top),
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
        println!();

        for (i, item) in items.iter().enumerate().skip(top).take(view) {
            if i == selected {
                paint(HIGHLIGHT);
                // Pad so the bar spans the row rather than hugging the text.
                let text = fit(&item.label, cols.saturating_sub(4));
                let width = cols.saturating_sub(5);
                println!("  {text:<width$} ");
                body();
            } else {
                println!("   {}", fit(&item.label, cols.saturating_sub(3)));
            }
        }
        if items.len() > view {
            paint(colors(Style::Dim));
            println!("   ... {} of {}", selected + 1, items.len());
            body();
        }

        println!();
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
    println!("{names}");
    body();

    print!("     ");
    for (i, step) in CONFIRM_SEQUENCE.iter().enumerate() {
        let (mark, style) = match i {
            _ if i < position => ("[x]", Style::Good),
            _ if i == position => ("[ ]", Style::Key),
            _ => ("[ ]", Style::Dim),
        };
        paint(colors(style));
        print!("{mark}");
        body();
        for _ in 0..step_name(*step).len().saturating_sub(1) {
            print!(" ");
        }
    }
    println!();
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
        println!();
        paint(colors(Style::Title));
        println!("  To authorise this, press in order:");
        body();
        println!();

        draw_steps(position);

        println!();
        if wrong {
            paint(colors(Style::Bad));
            println!("  wrong button - sequence reset");
            body();
        } else if position > 0 {
            paint(colors(Style::Key));
            println!("  next: {}", step_name(CONFIRM_SEQUENCE[position]));
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
