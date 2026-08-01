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
//! reads answers back.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
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

pub fn clear() {
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

fn set_color(fg: Color, bg: Color) {
    system::with_stdout(|out| {
        let _ = out.set_color(fg, bg);
    });
}

const NORMAL: (Color, Color) = (Color::LightGray, Color::Black);
const HIGHLIGHT: (Color, Color) = (Color::Black, Color::LightGray);

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

fn rule(columns: usize) -> String {
    "-".repeat(columns.saturating_sub(1).min(78))
}

fn header(title: &str) {
    let (cols, _) = size();
    at(0, 0);
    println!("{}", fit(title, cols));
    println!("{}", rule(cols));
}

/// One selectable line, plus whatever should be shown about it while it is
/// selected.
pub struct Item {
    pub label: String,
    pub detail: Vec<String>,
}

impl Item {
    pub fn with_detail(label: impl Into<String>, detail: Vec<String>) -> Self {
        Item { label: label.into(), detail }
    }
}

/// A scrollable page of text, ending in "continue" or "cancel".
///
/// Returns false if the operator backed out. A Deck cannot scroll back, so
/// anything longer than the screen has to be navigable rather than simply
/// printed and lost.
pub fn page(title: &str, lines: &[String]) -> bool {
    let (cols, rows) = size();
    let view = rows.saturating_sub(6).max(4);
    let mut top = 0usize;
    let max_top = lines.len().saturating_sub(view);
    drain();

    loop {
        clear();
        header(title);
        for line in lines.iter().skip(top).take(view) {
            println!("{}", fit(line, cols));
        }
        at(0, rows.saturating_sub(2));
        if lines.len() > view {
            println!(
                "  lines {}-{} of {}   D-pad up/down to scroll",
                top + 1,
                (top + view).min(lines.len()),
                lines.len()
            );
        }
        print!("  A = continue    B = back");

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
pub fn message(title: &str, lines: &[String]) {
    let (cols, rows) = size();
    clear();
    header(title);
    for line in lines {
        println!("{}", fit(line, cols));
    }
    at(0, rows.saturating_sub(2));
    print!("  A = continue");
    drain();
    loop {
        match wait() {
            Input::Select | Input::Cancel => return,
            _ => {}
        }
    }
}

/// A D-pad menu with a highlight bar. Returns the chosen index, or `None`
/// if the operator backed out.
///
/// `hint` names what B does here, which differs between the top level
/// ("exit") and a submenu ("back").
pub fn menu(title: &str, intro: &[String], items: &[Item], hint: &str) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let mut selected = 0usize;
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
            println!("{}", fit(line, cols));
        }
        println!();

        for (i, item) in items.iter().enumerate().skip(top).take(view) {
            if i == selected {
                set_color(HIGHLIGHT.0, HIGHLIGHT.1);
                // Pad so the bar spans the row rather than hugging the text.
                let text = fit(&item.label, cols.saturating_sub(4));
                let width = cols.saturating_sub(5);
                println!("  {text:<width$} ");
                set_color(NORMAL.0, NORMAL.1);
            } else {
                println!("   {}", fit(&item.label, cols.saturating_sub(3)));
            }
        }
        if items.len() > view {
            println!("   ... {} of {}", selected + 1, items.len());
        }

        println!();
        for line in &items[selected].detail {
            println!("{}", fit(line, cols));
        }

        at(0, rows.saturating_sub(2));
        print!("  D-pad = move    A = choose    {hint}");

        match wait() {
            Input::Up => {
                selected = if selected == 0 { items.len() - 1 } else { selected - 1 };
            }
            Input::Down => selected = (selected + 1) % items.len(),
            Input::Select => return Some(selected),
            Input::Cancel => return None,
            _ => {}
        }
    }
}

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

/// Require [`CONFIRM_SEQUENCE`] before a destructive write.
///
/// Any wrong press resets progress. B cancels outright.
pub fn confirm_sequence(title: &str, warning: &[String]) -> bool {
    let (cols, rows) = size();
    let mut position = 0usize;
    let mut wrong = false;
    drain();

    loop {
        clear();
        header(title);
        for line in warning {
            println!("{}", fit(line, cols));
        }
        println!();
        println!("  To authorise this, press in order:");
        println!();

        let mut names = String::from("     ");
        let mut marks = String::from("     ");
        for (i, step) in CONFIRM_SEQUENCE.iter().enumerate() {
            let name = step_name(*step);
            names.push_str(name);
            names.push_str("  ");
            let mark = if i < position { "[x]" } else { "[ ]" };
            marks.push_str(mark);
            for _ in 0..name.len().saturating_sub(1) {
                marks.push(' ');
            }
        }
        println!("{names}");
        println!("{marks}");
        println!();
        if wrong {
            println!("  wrong button - sequence reset");
        } else if position > 0 {
            println!("  next: {}", step_name(CONFIRM_SEQUENCE[position]));
        }

        at(0, rows.saturating_sub(2));
        print!("  B = cancel, nothing is written");

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
