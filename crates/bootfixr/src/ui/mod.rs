//! Screen handling and the operator's side of the conversation.
//!
//! Built for a Steam Deck in firmware, which means: no keyboard, no
//! scrollback, and the measured input set from `docs/steamdeck-input.log` —
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
//! Which way up that picture goes, and how big its text is, are settled by
//! [`display`], which View reaches from any screen that waits for a press.
//! It is the one control that is not about the disk, and it has to be
//! available from wherever the operator happens to be when they find they
//! cannot read the screen.
//!
//! Report *formatting* lives in `gptcore::report`, which has no UEFI
//! dependency and is tested on the host. This module only paints lines and
//! reads answers back. Which colour a line gets is decided by the
//! [`Style`] the writer attached to it, never by inspecting the text here:
//! rewording a message must not silently change what it looks like.

pub(crate) mod term;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
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
    /// The View button, which arrives as TAB.
    View,
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
            '\t' => Some(Input::View),
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
fn wait_raw() -> Input {
    term::flush();
    loop {
        if let Some(i) = poll() {
            return i;
        }
        boot::stall(TICK_US);
    }
}

/// Wait, handling View here so that every screen gets [`display`] without
/// asking for it.
///
/// Someone who cannot read the screen needs to fix it from wherever they
/// are, not from wherever we decided the setting lived. The press is still
/// reported back, because every screen that waits redraws on whatever it
/// gets, and one that has just been drawn at a different size or a quarter
/// turn round needs that redraw more than most.
///
/// Screens that read View themselves poll with [`wait_raw`] instead: a menu
/// offering a closer look at the selected row, and [`display`], which would
/// otherwise open on top of itself.
fn wait() -> Input {
    let input = wait_raw();
    if input == Input::View {
        display();
    }
    input
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
/// The button names in the key hints along the bottom.
///
/// A colour used nowhere else, and deliberately so. Sat in `Dim`, the hints
/// were read as chrome and skipped: test sessions had operators who never
/// found View, and who were unsure which button committed a choice and
/// which backed out of it. Nothing else on the screen names a physical
/// button, so a colour that means only that costs no ambiguity elsewhere.
const HINT_KEY: (Color, Color) = (Color::LightMagenta, Color::Black);

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

/// Re-detect the display after control briefly left the program.
///
/// A chainloaded image that returns instead of taking the machine over is
/// free to have called `SetMode` on the GOP first, so the framebuffer this
/// program cached before handing control over cannot be trusted. This is
/// the same probe [`init`] does, run again rather than once: cheap, and it
/// is what makes the menu safe to redraw afterwards.
pub fn redisplay() {
    hide_cursor();
    term::init();
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
/// Callers wrap once, where they build their lines, rather than on every
/// repaint. [`display`] can change the width under a screen that is already
/// up; [`fit`] then cuts whatever no longer fits, and the next screen to
/// build its lines wraps them to the width now in force. Re-wrapping in
/// place would mean handing the width to every writer in `gptcore`, which
/// is a large price for a rescue screen nobody visits twice.
pub fn wrapped(text: &str, style: Style, hang: &str) -> Vec<Line> {
    gptcore::style::wrap(text, text_width(size().0), style, hang)
}

fn fit(line: &str, columns: usize) -> String {
    let limit = text_width(columns);
    // Control characters are neutralised in `gptcore::style::Line::new`,
    // and again here, because a menu row's label is a bare `String` that
    // never becomes a `Line` — and `pick_boot_entry` builds one out of a
    // boot entry's own description.
    let safe = line.chars().map(|c| if c.is_control() { '.' } else { c });
    if line.chars().count() <= limit {
        return safe.collect();
    }
    let mut out: String = safe.take(limit.saturating_sub(1)).collect();
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

// ------------------------------------------------------------- the subject

/// Single-threaded interior mutability, as in [`term`]: a UEFI application
/// owns the machine, and nothing else can reach this.
struct Subject(UnsafeCell<Option<String>>);

// SAFETY: as above.
unsafe impl Sync for Subject {}

static SUBJECT: Subject = Subject(UnsafeCell::new(None));

fn with_subject<R>(f: impl FnOnce(Option<&str>) -> R) -> R {
    // SAFETY: single-threaded, and `f` only draws — nothing below it sets
    // the subject while this borrow is live.
    let slot = unsafe { &*SUBJECT.0.get() };
    f(slot.as_deref())
}

fn set_subject(value: Option<String>) {
    // SAFETY: as above, and no borrow from `with_subject` outlives its call.
    unsafe { *SUBJECT.0.get() = value };
}

/// Names what every screen is about, under the title, until it is dropped.
///
/// The disk picker is skipped when there is only one disk, so a screen
/// proposing to overwrite a partition table can no longer rely on the
/// operator having just chosen the disk by hand. Naming it in the header
/// puts that back, on every screen of the operation rather than in a line
/// of body text each screen has to remember to write.
///
/// A guard rather than a pair of calls because these operations leave by
/// half a dozen early returns apiece, and a header still naming the last
/// disk on a screen about something else would be worse than no header at
/// all. Hold one at a time: the innermost drop clears the header. Bind it
/// to a name — `let _on = ui::working_on(..)` — since `let _ =` would drop
/// it before the first screen is drawn.
#[must_use = "the header only names it while this is held"]
pub struct Subtitle(());

pub fn working_on(what: impl Into<String>) -> Subtitle {
    set_subject(Some(what.into()));
    Subtitle(())
}

impl Drop for Subtitle {
    fn drop(&mut self) {
        set_subject(None);
    }
}

/// Rows the header occupies: the title, the subject if there is one, and
/// the rule. Every screen that has to know how much room is left asks.
fn header_rows() -> usize {
    with_subject(|subject| 2 + usize::from(subject.is_some()))
}

fn header(title: &str) {
    let (cols, _) = size();
    at(0, 0);
    paint(colors(Style::Title));
    outln!("{}", fit(title, cols));
    with_subject(|subject| {
        if let Some(text) = subject {
            // Key, because this is the thing on the screen that must
            // actually be taken in: the disk about to be written to.
            paint(colors(Style::Key));
            outln!("  {}", fit(text, cols.saturating_sub(2)));
        }
    });
    paint(colors(Style::Dim));
    outln!("{}", rule(cols));
    body();
}

/// One button, and what it does on this screen.
///
/// A pair rather than a formatted string because the two halves are painted
/// differently, and working out which half is the button by looking at the
/// text is exactly what the module note forbids.
struct Hint<'a> {
    key: &'a str,
    action: &'a str,
}

const fn hint<'a>(key: &'a str, action: &'a str) -> Hint<'a> {
    Hint { key, action }
}

/// Whether this session is running on a Steam Deck, as [`init`] settled it.
///
/// A Deck has a D-pad and an A, B and View button and no keyboard; anything
/// else that reaches this firmware has a keyboard and none of those. Set
/// once, since the SMBIOS table a machine reports does not change mid-boot.
static STEAM_DECK: AtomicBool = AtomicBool::new(false);

/// What the footer calls a button, on this machine.
///
/// [`poll`] already folds a keyboard's Enter, Escape, Tab and arrow keys
/// into the same [`Input`] the Deck's pad sends, so the two are
/// interchangeable everywhere but here: the one place that tells the
/// operator what to press has to name what they actually have in hand.
/// Anything not named below — `LEFT/RIGHT`, `UP/DOWN` — reads the same on
/// both, so it passes through.
///
/// Public within the crate as well as used by [`footer`]: a few screens
/// name a button in their own body text rather than only in the footer, and
/// those have to agree with it.
pub(crate) fn key_label(key: &str) -> &str {
    if STEAM_DECK.load(Ordering::Relaxed) {
        return key;
    }
    match key {
        "A" => "Enter",
        "B" => "Escape",
        "View" => "Tab",
        "D-pad" => "Arrows",
        other => other,
    }
}

/// The configure display screen's hint, where View leads there at all.
///
/// On the firmware's own text console the orientation and the font belong to
/// the firmware, [`display`] returns without drawing, and a footer must not
/// offer what View will not do.
fn display_hint() -> Option<Hint<'static>> {
    term::rotation().map(|_| hint("View", "configure display"))
}

/// The key hints along the bottom: what the operator can press, and what it
/// will do.
///
/// Set off from the content by a rule and given a colour of its own, because
/// this is the one row on every screen that has to be found without being
/// looked for. It is still the least loud thing on the screen — the button
/// names carry [`HINT_KEY`] and nothing else does, and what they do is
/// plain body text rather than the `Dim` this used to be drawn in entirely.
///
/// Hints that will not fit are dropped rather than wrapped: a hint spilling
/// onto the last row would push the whole footer out of the place operators
/// learn to look at.
fn footer(rows: usize, hints: &[Hint]) {
    let (cols, _) = size();
    let limit = text_width(cols);

    at(0, rows.saturating_sub(3));
    paint(colors(Style::Dim));
    out!("{}", rule(cols));

    at(0, rows.saturating_sub(2));
    body();
    let mut used = 0usize;
    for (i, hint) in hints.iter().enumerate() {
        let key = key_label(hint.key);
        // The margin before the first, the gap between the rest, then
        // "[key] action".
        let gap = if i == 0 { 2 } else { 3 };
        let width = gap + key.chars().count() + 3 + hint.action.chars().count();
        if used + width > limit {
            break;
        }
        for _ in 0..gap {
            out!(" ");
        }
        paint(HINT_KEY);
        out!("[{}]", key);
        body();
        out!(" {}", hint.action);
        used += width;
    }
    body();
}

// -------------------------------------------------------------------- menu

/// One line of a menu: usually selectable, occasionally a blank spacer
/// that groups the rows around it.
pub struct Item {
    pub label: String,
    pub detail: Vec<Line>,
    pub selectable: bool,
}

impl Item {
    pub fn with_detail(label: impl Into<String>, detail: Vec<Line>) -> Self {
        Item { label: label.into(), detail, selectable: true }
    }

    /// A blank row the D-pad skips over, for splitting a menu into groups
    /// without a submenu neither group needs.
    pub fn separator() -> Self {
        Item { label: String::new(), detail: Vec::new(), selectable: false }
    }
}

/// Lines moved by one press of up or down in a report.
///
/// Not one. The Deck's D-pad repeats at about 1.8/s while held — measured,
/// in `docs/steamdeck-input.log` — so a line at a time makes a long report a
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
    let mut top = 0usize;
    drain();

    loop {
        // Measured each time round: the display screen is reachable from
        // here, and a page drawn for the old grid would be the wrong length.
        let (cols, rows) = size();
        // The header, the scroll indicator, the footer's rule, the footer,
        // and the row below it.
        let view = rows.saturating_sub(header_rows() + 4).max(4);
        let max_top = lines.len().saturating_sub(view);
        top = top.min(max_top);

        clear();
        header(title);
        for line in lines.iter().skip(top).take(view) {
            styled(line, cols);
        }
        if lines.len() > view {
            // Above the footer's rule, which owns the row below this one.
            at(0, rows.saturating_sub(4));
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
        let mut keys = alloc::vec![hint("A", "continue"), hint("B", "back")];
        keys.extend(display_hint());
        footer(rows, &keys);

        match wait() {
            Input::Up => top = top.saturating_sub(SCROLL_LINES),
            Input::Down => top = (top + SCROLL_LINES).min(max_top),
            Input::Left => top = top.saturating_sub(view),
            Input::Right => top = (top + view).min(max_top),
            Input::Select => return true,
            Input::Cancel => return false,
            // Already handled: the display screen has been and gone, and
            // this pass repaints the page underneath it.
            Input::View => {}
        }
    }
}

/// A message with a single acknowledgement.
pub fn message(title: &str, lines: &[Line]) {
    drain();
    loop {
        let (cols, rows) = size();
        clear();
        header(title);
        for line in lines {
            styled(line, cols);
        }
        let mut keys = alloc::vec![hint("A", "continue")];
        keys.extend(display_hint());
        footer(rows, &keys);

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
/// `back` names what B does here, which differs between the top level
/// ("exit") and a submenu ("back"). It is the action alone: the footer draws
/// the button.
pub fn menu(title: &str, intro: &[Line], items: &[Item], back: &str) -> Option<usize> {
    match run_menu(title, intro, items, back, None, 0) {
        Choice::Item(i) => Some(i),
        _ => None,
    }
}

/// A menu whose entries can also be opened for a closer look.
///
/// `inspect` names what View does with the selected row, as `back` does for
/// B. `start` is the initially selected row, so returning from an inspection
/// puts the operator back where they were rather than at the top.
pub fn menu_inspectable(
    title: &str,
    intro: &[Line],
    items: &[Item],
    back: &str,
    inspect: &str,
    start: usize,
) -> Choice {
    run_menu(title, intro, items, back, Some(inspect), start)
}

/// The next selectable row from `from`, moving by `dir` (1 down, -1 up) and
/// wrapping. Separators are stepped over rather than landed on, so the
/// D-pad never has to be pressed twice to clear one.
fn step(items: &[Item], from: usize, dir: isize) -> usize {
    let len = items.len() as isize;
    let mut i = from as isize;
    loop {
        i = (i + dir).rem_euclid(len);
        if items[i as usize].selectable {
            return i as usize;
        }
    }
}

fn run_menu(
    title: &str,
    intro: &[Line],
    items: &[Item],
    back: &str,
    inspect: Option<&str>,
    start: usize,
) -> Choice {
    if items.is_empty() {
        return Choice::Cancelled;
    }
    let mut selected = start.min(items.len() - 1);
    if !items[selected].selectable {
        selected = step(items, selected, 1);
    }
    let mut top = 0usize;
    drain();

    loop {
        let (cols, rows) = size();
        let detail_rows = items.iter().map(|i| i.detail.len()).max().unwrap_or(0);
        // header, intro, blank, [items], blank, detail, rule, hint, blank.
        let overhead = header_rows() + intro.len() + 1 + detail_rows + 4;
        let mut view = rows.saturating_sub(overhead).clamp(1, items.len());
        // A menu that scrolls also prints the "... n of m" indicator row,
        // which the budget above does not include; without giving it a row
        // of its own, the footer's rule lands on the selected item's last
        // detail line. Shrinking the window never makes the menu fit, so
        // the indicator is shown in exactly the runs that reserve for it.
        if items.len() > view {
            view = rows.saturating_sub(overhead + 1).clamp(1, items.len());
        }

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
                // Same left margin as an unselected row: the marker fills
                // the slot a space would otherwise take, rather than
                // shifting the text. Padded so the bar spans the row.
                let width = cols.saturating_sub(3);
                let text = fit(&item.label, width);
                outln!(" {} {text:<width$}", term::marker());
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

        let mut keys = alloc::vec![hint("D-pad", "move"), hint("A", "choose")];
        match inspect {
            Some(action) => keys.push(hint("View", action)),
            None => keys.extend(display_hint()),
        }
        keys.push(hint("B", back));
        footer(rows, &keys);

        // Raw, because a menu offering a closer look wants View for that;
        // where it does not, this reaches the display screen itself.
        match wait_raw() {
            Input::Up => selected = step(items, selected, -1),
            Input::Down => selected = step(items, selected, 1),
            Input::Select => return Choice::Item(selected),
            Input::Cancel => return Choice::Cancelled,
            Input::View => match inspect {
                Some(_) => return Choice::Inspect(selected),
                None => display(),
            },
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
        Input::View => "VIEW",
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
    let mut position = 0usize;
    let mut wrong = false;
    drain();

    loop {
        let (cols, rows) = size();
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

        let mut keys = alloc::vec![hint("B", "cancel, nothing is written")];
        keys.extend(display_hint());
        footer(rows, &keys);

        let input = wait();
        if input == Input::Cancel {
            return false;
        }
        // View has just been off to the display screen and back. Making
        // a legitimate reach for legibility also cost you your place in
        // the sequence would be gratuitous; it is not one of the presses.
        if input == Input::View {
            continue;
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

/// Choose a backend, settle which way up the screen goes, and remember
/// whether SMBIOS named this machine a Steam Deck.
///
/// Call once, before anything else draws. The orientation guessed from the
/// framebuffer's shape is taken as the answer and the session opens on the
/// menu; [`display`] is where it gets argued with, at whatever point the
/// operator decides it is wrong. `is_steam_deck` settles [`key_label`] the
/// same way, for the whole session.
pub fn init(is_steam_deck: bool) {
    STEAM_DECK.store(is_steam_deck, Ordering::Relaxed);

    // Before taking the framebuffer, not after: the firmware's console is
    // about to have its screen drawn on, and anything it puts there on the
    // way out should land where our first fill will wipe it.
    hide_cursor();

    // No usable framebuffer means the firmware's console, in whatever
    // orientation the firmware has it — which is the situation this tool
    // shipped in, and still works.
    term::init();
}

/// Turn the picture, and change how much of it fits.
///
/// Reachable with View from any screen that waits for a press, because the
/// moment this is wanted is the moment the screen cannot be read, and that
/// can happen anywhere: [`crate::gfx::Framebuffer::guess_rotation`] guessing
/// wrong on unfamiliar hardware, or simply a report of device paths that
/// wants more columns than the menu it was reached from. It used to be a
/// timed screen at startup, which charged every launch for a correction
/// almost nobody needed and still offered it only once.
///
/// LEFT and RIGHT work regardless of which way round the text came out,
/// which is the only reason this screen can rescue a wrong guess at all.
/// UP and DOWN step the text size, because how big is comfortable is a
/// judgement about a particular person looking at a particular panel from
/// wherever they happen to be holding it, and no amount of arithmetic here
/// settles it.
/// The modes the firmware says it could set, and what the largest would give.
///
/// Here because the tool deliberately leaves the firmware's own choice of
/// mode alone — see [`crate::gfx::Framebuffer::open`] — which means a screen
/// laid out in the smallest cell might be a display that can do no better,
/// or might be a firmware that picked 800x600 on a panel capable of far
/// more. Those look identical from the operator's chair, and they are not
/// the same problem: in the second, both text sizes are refused, because
/// nothing above 8x16 reaches 80 columns at that resolution, and a font
/// that will not change reads as a font that was never there.
///
/// Reporting only. Nothing on this screen calls `SetMode`.
fn display_modes(
    modes: &[term::Mode],
    upgrade: Option<&term::Upgrade>,
    cols: usize,
    rows: usize,
    layout: Option<term::Layout>,
) {
    if modes.is_empty() {
        return;
    }
    // Rows already spent: the header, four of prose, two blanks, the two
    // status lines, the pixel line if it was printed, and the blank and
    // heading below. Three more are held back for the two lines the offer
    // takes and for the note a press can add. What is left must stop short
    // of the footer's rule.
    let used = header_rows() + 10 + usize::from(layout.is_some());
    let budget = rows.saturating_sub(3).saturating_sub(used + 3).max(1);

    outln!();
    paint(colors(Style::Dim));
    let undrawable = modes.iter().any(|m| !m.drawable);
    outln!(
        "  Modes this display offers (* in use{}):",
        if undrawable { ", ! not ours to draw in" } else { "" }
    );
    body();
    for line in mode_lines(modes, text_width(cols).saturating_sub(4), budget) {
        outln!("    {line}");
    }

    // The offer, where there is one worth making. Said in full — the mode,
    // the grid it comes to, and how it undoes itself — because an operator
    // about to risk the only picture they have needs to know the way back
    // before the picture is gone, not after.
    if let Some(upgrade) = upgrade {
        paint(colors(Style::Dim));
        outln!(
            "  [{}] tries {} x {}: {} x {} characters in cells of {}x{}.",
            key_label("View"),
            upgrade.width,
            upgrade.height,
            upgrade.layout.cols,
            upgrade.layout.rows,
            upgrade.layout.cell_w,
            upgrade.layout.cell_h
        );
        outln!("  If the picture does not survive it, waiting brings this one back.");
        body();
    }
}

/// The mode list, packed into as few rows as the width allows.
fn mode_lines(modes: &[term::Mode], width: usize, rows: usize) -> Vec<String> {
    let entries: Vec<String> = modes
        .iter()
        .map(|m| {
            let mark = if m.current {
                "*"
            } else if m.drawable {
                ""
            } else {
                "!"
            };
            format!("{}x{}{mark}", m.width, m.height)
        })
        .collect();

    let (lines, shown) = pack(&entries, width, rows);
    if shown == entries.len() {
        return lines;
    }
    // A list quietly cut short reads as the whole truth, so one of the rows
    // goes to saying how much of it is missing.
    let (mut lines, shown) = pack(&entries, width, rows.saturating_sub(1));
    lines.push(format!("+{} more", entries.len() - shown));
    lines
}

/// Greedily fill at most `rows` lines of at most `width` characters,
/// reporting how many entries fitted.
fn pack(entries: &[String], width: usize, rows: usize) -> (Vec<String>, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut shown = 0;
    for entry in entries {
        // Two spaces between entries, so a row of them reads as a list
        // rather than as one long number.
        let room = |last: &&mut String| last.chars().count() + 2 + entry.chars().count() <= width;
        if let Some(last) = lines.last_mut().filter(room) {
            last.push_str("  ");
            last.push_str(entry);
        } else if lines.len() < rows {
            lines.push(entry.clone());
        } else {
            break;
        }
        shown += 1;
    }
    (lines, shown)
}

/// Said when a press finds the end of the cell-size ladder.
///
/// One text per direction, and they name the direction rather than say
/// "no further that way". Where a single cell size is all that fits — which
/// is the case this screen most needs to explain — every press lands on one
/// of these, and a single shared wording would sit there unchanged however
/// the operator pressed, which is indistinguishable from a screen that has
/// stopped reading the buttons at all. Two texts alternate, so the display
/// visibly answers each press.
///
/// Both blame the resolution, because that is what is doing it, and the mode
/// offer sitting a line or two above is what can be done about it.
const NO_BIGGER_SIZE: &str = "No bigger font available at this resolution.";
const NO_SMALLER_SIZE: &str = "No smaller font available at this resolution.";

/// Seconds a mode change is given to be confirmed in before it is undone.
///
/// Long enough to take in a screen that has just changed shape and press one
/// button; short enough that somebody looking at a black panel is not left
/// wondering whether the tool has hung. The operator was told what was about
/// to happen before it happened, so this is a reaction, not a decision.
const MODE_SECONDS: usize = 6;

/// Put a mode change to the operator, and report whether they confirmed it.
///
/// The confirmation has to be a keypress, and silence has to count as a no.
/// A mode the panel will not display looks, from in here, exactly like one it
/// will: the firmware reports success either way. So the question is asked on
/// the new mode itself — if it can be read, it works, and if nothing comes
/// back the only safe reading is that nobody can see it.
///
/// Only A and B are answers. The arrows are ignored, and so is View: the
/// press that opened this screen repeats while held on a Deck, and a
/// confirmation a held button can give is not a confirmation.
fn confirm_mode(previous: (usize, usize)) -> bool {
    let ticks_per_second = 1_000_000 / TICK_US;
    for left in (1..=MODE_SECONDS).rev() {
        let (cols, rows) = size();
        clear();
        header("Display");
        paint(colors(Style::Key));
        outln!("  This is a new display mode, {cols} x {rows} characters.");
        body();
        outln!();
        outln!("  If you can read it, keep it. If you cannot, wait: the mode");
        outln!("  you had comes back on its own and nothing is lost by it.");
        outln!();
        paint(colors(Style::Warn));
        let (width, height) = previous;
        outln!("  Going back to {width} x {height} in {left}...");
        body();
        footer(rows, &[hint("A", "keep this mode"), hint("B", "go back now")]);

        term::flush();
        for _ in 0..ticks_per_second {
            match poll() {
                Some(Input::Select) => return true,
                Some(Input::Cancel) => return false,
                _ => boot::stall(TICK_US),
            }
        }
    }
    false
}

/// Ask the firmware for a larger framebuffer mode, keeping it only if the
/// operator can see it. Returns what the display screen should say, if
/// anything.
///
/// Every way this can go wrong ends with the operator looking at a picture
/// they can read: a mode the firmware refuses changes nothing, a mode this
/// program cannot draw in is put back by [`crate::gfx::Framebuffer::set_mode`]
/// before it returns, and a mode that reaches nothing is put back by the
/// clock. The one irrecoverable case — a switch that works, is not confirmed,
/// and cannot be undone — is reported rather than papered over, though there
/// is by then nobody who can read the report.
fn try_mode(width: usize, height: usize) -> Option<&'static str> {
    let previous = term::resolution()?;
    // View repeats while held, and the confirmation must not be answered by
    // the press that got here.
    drain();

    if !term::set_mode(width, height) {
        return Some("The firmware would not set that mode.");
    }
    if confirm_mode(previous) {
        return None;
    }
    if term::set_mode(previous.0, previous.1) {
        return Some("That mode went unconfirmed, so this one is back.");
    }
    Some("That mode went unconfirmed, and the old one will not come back.")
}

fn display() {
    let Some(mut rotation) = term::rotation() else {
        return;
    };
    // Asked once, then again after any mode change: `QueryMode` allocates on
    // every call and this screen redraws on every press, but which mode is
    // the current one is baked into the answer, so it does go stale when the
    // one thing here that can change it does.
    let mut modes = term::modes();
    let mut note: Option<&'static str> = None;
    drain();

    loop {
        let (cols, rows) = size();
        let layout = term::layout();
        let upgrade = term::upgrade(&modes);
        clear();
        header("Display");
        outln!("  This screen is drawn by the toolkit itself rather than by the");
        outln!("  firmware, which is what makes it possible to turn the picture");
        outln!("  to match the panel. The Steam Deck's is mounted sideways, so");
        outln!("  the firmware's own text comes out a quarter turn off there.");
        outln!();
        paint(colors(Style::Key));
        outln!("  Now showing: {}, {cols} x {rows} characters", fit(rotation.name(), cols));
        body();
        if let (Some((width, height)), Some(layout)) = (term::resolution(), layout) {
            outln!(
                "  Drawn on {width} x {height} pixels, cells of {}x{}",
                layout.cell_w,
                layout.cell_h
            );
        }
        outln!();
        outln!("  If you can read this the right way up, there is nothing to do.");

        display_modes(&modes, upgrade.as_ref(), cols, rows, layout);

        if let Some(note) = note {
            paint(colors(Style::Dim));
            outln!("  {}", fit(note, cols));
            body();
        }

        // View earns a hint only where it does something: a Steam Deck has
        // no button left to give this — X, Y, the bumpers, the triggers and
        // the back buttons all report nothing at all, measured in
        // `docs/steamdeck-input.log` — so the button that opens this screen
        // is the button that acts on it, and it keeps its old meaning of
        // leaving on every screen where there is no mode worth trying.
        let mut hints = vec![hint("LEFT/RIGHT", "turn"), hint("UP/DOWN", "text size")];
        if upgrade.is_some() {
            hints.push(hint("View", "bigger mode"));
        }
        hints.push(hint("A", "done"));
        footer(rows, &hints);

        note = None;
        // Raw, or View would open this screen on top of itself.
        match wait_raw() {
            Input::Left => rotation = rotation.previous(),
            Input::Right => rotation = rotation.next(),
            Input::Up => note = (!term::resize_text(true)).then_some(NO_BIGGER_SIZE),
            Input::Down => note = (!term::resize_text(false)).then_some(NO_SMALLER_SIZE),
            Input::View => match upgrade {
                Some(upgrade) => {
                    note = try_mode(upgrade.width, upgrade.height);
                    // Which mode is the current one is part of the answer,
                    // and so of the offer built from it.
                    modes = term::modes();
                }
                // Nothing worth trying, so View means what it means
                // everywhere else on this screen.
                None => break,
            },
            Input::Select | Input::Cancel => break,
        }
        term::set_rotation(rotation);
    }
    // The press that leaves is held long enough to repeat as often as any
    // other, and the screen this returns to would act on what it left behind.
    drain();
}
