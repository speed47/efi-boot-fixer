//! Input probe: what does the hardware actually send in the UEFI
//! environment?
//!
//! A Steam Deck has no built-in keyboard, and Bluetooth is not available in
//! firmware, so the repair tool cannot ask anyone to type a confirmation
//! word. What it *can* use depends on how the firmware exposes the buttons,
//! sticks, trackpads and touchscreen, and that is not something to guess at.
//!
//! The probe walks a scripted list of controls, one at a time, and
//! attributes whatever arrives to the control it just asked for. That beats
//! a free-form capture, where someone afterwards has to correlate a wall of
//! timestamps against what their thumbs were doing.
//!
//! Everything is written to `efiprobe.log` on the ESP it was launched from,
//! flushed after every line, as well as to the screen. The screen scrolls
//! and cannot be copied off the device; the file can be read from Linux
//! afterwards, and cutting the power keeps whatever was logged so far.
//!
//! Two things learned the hard way from a first run on real hardware:
//!
//! * B and the burger button both report as ESCAPE, so ESCAPE must not be
//!   an exit key here, or those two controls cannot be tested at all.
//! * `FileMode::CreateReadWrite` does not truncate, so a shorter second run
//!   leaves the tail of a longer first one spliced onto the end. The old
//!   file is now deleted explicitly.
//!
//! Pointer and touch samples are summarised per step rather than logged
//! individually: a trackpad emits hundreds of events per second and buries
//! everything else.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use uefi::boot::{self, ScopedProtocol, SearchType};
use uefi::prelude::*;
use uefi::proto::console::pointer::Pointer;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::proto::media::file::{File, FileAttribute, FileMode, RegularFile};
use uefi::proto::unsafe_protocol;
use uefi::{cstr16, system};
use uefi_raw::protocol::console::{AbsolutePointerProtocol, AbsolutePointerState};

/// Touchscreen. Not wrapped by the `uefi` crate, so bound here directly.
#[repr(transparent)]
#[unsafe_protocol(AbsolutePointerProtocol::GUID)]
struct AbsolutePointer(AbsolutePointerProtocol);

impl AbsolutePointer {
    fn describe(&self) -> String {
        // SAFETY: `mode` was filled in by the firmware when the protocol was
        // installed, and stays valid while the protocol is open.
        unsafe {
            let m = self.0.mode;
            if m.is_null() {
                return "<null mode>".to_string();
            }
            let m = &*m;
            format!(
                "x {}..{}, y {}..{}, z {}..{}, attrs {:#x}",
                m.absolute_min_x,
                m.absolute_max_x,
                m.absolute_min_y,
                m.absolute_max_y,
                m.absolute_min_z,
                m.absolute_max_z,
                m.attributes.bits()
            )
        }
    }

    fn poll(&mut self) -> Option<AbsolutePointerState> {
        let mut state = AbsolutePointerState::default();
        // SAFETY: live protocol interface, and `state` is a local we own.
        let status = unsafe { (self.0.get_state)(&mut self.0, &mut state) };
        (status == Status::SUCCESS).then_some(state)
    }
}

const TICK_US: usize = 5_000; // 5 ms
const TICKS_PER_SEC: u32 = (1_000_000 / TICK_US) as u32;
const STEP_SECONDS: u32 = 4;
const FREEFORM_SECONDS: u32 = 20;

/// The controls to walk through, in order.
const STEPS: &[&str] = &[
    "A",
    "B",
    "X",
    "Y",
    "D-pad UP",
    "D-pad DOWN",
    "D-pad LEFT",
    "D-pad RIGHT",
    "L1 (left bumper)",
    "R1 (right bumper)",
    "L2 (left trigger, pull fully)",
    "R2 (right trigger, pull fully)",
    "View button (two rectangles)",
    "Menu / burger button",
    "STEAM button",
    "QAM button (three dots)",
    "L4 (back, upper left)",
    "L5 (back, lower left)",
    "R4 (back, upper right)",
    "R5 (back, lower right)",
    "LEFT STICK: click it in",
    "RIGHT STICK: click it in",
    "LEFT STICK: push fully right and hold",
    "RIGHT STICK: push fully right and hold",
    "LEFT TRACKPAD: swipe around, then click",
    "RIGHT TRACKPAD: swipe around, then click",
    "TOUCHSCREEN: tap the centre of the screen",
    "TOUCHSCREEN: drag a finger across the screen",
    "HOLD A down for this whole step (auto-repeat test)",
    "HOLD D-pad DOWN for this whole step (auto-repeat test)",
];

#[derive(Default)]
struct Range {
    seen: bool,
    min: i64,
    max: i64,
}

impl Range {
    fn add(&mut self, v: i64) {
        if !self.seen {
            self.seen = true;
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
    }

    fn show(&self) -> String {
        if self.seen {
            format!("{}..{}", self.min, self.max)
        } else {
            "-".to_string()
        }
    }
}

/// What arrived during one step.
#[derive(Default)]
struct Tally {
    unicode: Vec<(u16, u32)>,
    scan: Vec<(u16, u32)>,
    ptr_events: Vec<u32>,
    ptr_x: Vec<Range>,
    ptr_y: Vec<Range>,
    ptr_z: Vec<Range>,
    ptr_left: Vec<bool>,
    ptr_right: Vec<bool>,
    touch_events: Vec<u32>,
    touch_x: Vec<Range>,
    touch_y: Vec<Range>,
    touch_z: Vec<Range>,
    touch_buttons: Vec<u32>,
}

fn bump(list: &mut Vec<(u16, u32)>, code: u16) {
    match list.iter_mut().find(|(c, _)| *c == code) {
        Some((_, n)) => *n += 1,
        None => list.push((code, 1)),
    }
}

impl Tally {
    fn new(pointers: usize, touches: usize) -> Self {
        let mut t = Tally::default();
        for _ in 0..pointers {
            t.ptr_events.push(0);
            t.ptr_x.push(Range::default());
            t.ptr_y.push(Range::default());
            t.ptr_z.push(Range::default());
            t.ptr_left.push(false);
            t.ptr_right.push(false);
        }
        for _ in 0..touches {
            t.touch_events.push(0);
            t.touch_x.push(Range::default());
            t.touch_y.push(Range::default());
            t.touch_z.push(Range::default());
            t.touch_buttons.push(0);
        }
        t
    }

    fn summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (code, n) in &self.unicode {
            let name = match code {
                0x0d => " (CR)",
                0x0a => " (LF)",
                0x09 => " (TAB)",
                0x08 => " (BACKSPACE)",
                0x20 => " (SPACE)",
                _ => "",
            };
            out.push(format!("    KEY unicode={code:#06x}{name} x{n}"));
        }
        for (code, n) in &self.scan {
            out.push(format!("    KEY scan={:#06x} {} x{}", code, scan_name(*code), n));
        }
        for i in 0..self.ptr_events.len() {
            if self.ptr_events[i] > 0 {
                out.push(format!(
                    "    PTR{i} events={} dx={} dy={} dz={} left={} right={}",
                    self.ptr_events[i],
                    self.ptr_x[i].show(),
                    self.ptr_y[i].show(),
                    self.ptr_z[i].show(),
                    self.ptr_left[i],
                    self.ptr_right[i]
                ));
            }
        }
        for i in 0..self.touch_events.len() {
            if self.touch_events[i] > 0 {
                out.push(format!(
                    "    TOUCH{i} events={} x={} y={} z={} buttons={:#x}",
                    self.touch_events[i],
                    self.touch_x[i].show(),
                    self.touch_y[i].show(),
                    self.touch_z[i].show(),
                    self.touch_buttons[i]
                ));
            }
        }
        if out.is_empty() {
            out.push("    (nothing)".to_string());
        }
        out
    }
}

fn scan_name(code: u16) -> &'static str {
    match code {
        0x01 => "UP",
        0x02 => "DOWN",
        0x03 => "RIGHT",
        0x04 => "LEFT",
        0x05 => "HOME",
        0x06 => "END",
        0x07 => "INSERT",
        0x08 => "DELETE",
        0x09 => "PAGE_UP",
        0x0a => "PAGE_DOWN",
        0x0b..=0x16 => "F-key",
        0x17 => "ESCAPE",
        _ => "?",
    }
}

/// Screen plus `efiprobe.log`, flushed every line so a power-off keeps it.
struct Log {
    file: Option<RegularFile>,
}

impl Log {
    fn open() -> Self {
        let file = (|| {
            let mut fs = boot::get_image_file_system(boot::image_handle()).ok()?;
            let mut root = fs.open_volume().ok()?;
            // CreateReadWrite does not truncate: a shorter run would leave
            // the tail of a longer previous one behind. Remove it first.
            if let Ok(old) =
                root.open(cstr16!("efiprobe.log"), FileMode::ReadWrite, FileAttribute::empty())
            {
                let _ = old.delete();
            }
            let handle = root
                .open(cstr16!("efiprobe.log"), FileMode::CreateReadWrite, FileAttribute::empty())
                .ok()?;
            handle.into_regular_file()
        })();
        Log { file }
    }

    fn line(&mut self, text: &str) {
        uefi::println!("{text}");
        if let Some(f) = self.file.as_mut() {
            let _ = f.write(text.as_bytes());
            let _ = f.write(b"\r\n");
            let _ = f.flush();
        }
    }
}

fn open_all<P: uefi::proto::ProtocolPointer + ?Sized>() -> Vec<ScopedProtocol<P>> {
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&P::GUID)) else {
        return Vec::new();
    };
    handles.iter().filter_map(|h| boot::open_protocol_exclusive::<P>(*h).ok()).collect()
}

/// Collect for `seconds`, attributing everything to the current step.
fn capture(
    seconds: u32,
    pointers: &mut [ScopedProtocol<Pointer>],
    touches: &mut [ScopedProtocol<AbsolutePointer>],
) -> Tally {
    let mut tally = Tally::new(pointers.len(), touches.len());
    for _ in 0..(seconds * TICKS_PER_SEC) {
        if let Some(key) = system::with_stdin(|stdin| stdin.read_key().ok().flatten()) {
            match key {
                Key::Printable(c) => bump(&mut tally.unicode, u16::from(c)),
                // NOTE: ESCAPE is deliberately not an exit; B and the burger
                // button both report as ESCAPE on a Deck.
                Key::Special(ScanCode(code)) => bump(&mut tally.scan, code),
            }
        }
        for (i, p) in pointers.iter_mut().enumerate() {
            if let Ok(Some(s)) = p.read_state() {
                tally.ptr_events[i] += 1;
                tally.ptr_x[i].add(s.relative_movement[0] as i64);
                tally.ptr_y[i].add(s.relative_movement[1] as i64);
                tally.ptr_z[i].add(s.relative_movement[2] as i64);
                tally.ptr_left[i] |= s.button[0];
                tally.ptr_right[i] |= s.button[1];
            }
        }
        for (i, t) in touches.iter_mut().enumerate() {
            if let Some(s) = t.poll() {
                tally.touch_events[i] += 1;
                tally.touch_x[i].add(s.current_x as i64);
                tally.touch_y[i].add(s.current_y as i64);
                tally.touch_z[i].add(s.current_z as i64);
                tally.touch_buttons[i] |= s.active_buttons;
            }
        }
        boot::stall(TICK_US);
    }
    tally
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("uefi helpers");
    let mut log = Log::open();

    log.line("=== efiprobe v2: guided UEFI input mapping ===");
    log.line(&format!(
        "firmware: {} rev {:#x}, UEFI {}.{}",
        system::firmware_vendor(),
        system::firmware_revision(),
        system::uefi_revision().major(),
        system::uefi_revision().minor()
    ));
    if log.file.is_none() {
        log.line("WARNING: could not create efiprobe.log on the ESP; screen only.");
    }

    let mut pointers = open_all::<Pointer>();
    let mut touches = open_all::<AbsolutePointer>();
    log.line(&format!("SimplePointer handles   : {}", pointers.len()));
    log.line(&format!("AbsolutePointer handles : {}", touches.len()));
    for (i, t) in touches.iter().enumerate() {
        log.line(&format!("  AbsolutePointer[{i}]: {}", t.describe()));
    }
    for (i, p) in pointers.iter().enumerate() {
        let m = p.mode();
        log.line(&format!(
            "  SimplePointer[{i}]: resolution x {} y {} z {} counts/mm, buttons L:{} R:{}",
            m.resolution[0], m.resolution[1], m.resolution[2], m.has_button[0], m.has_button[1]
        ));
    }

    let total = STEPS.len() as u32 * STEP_SECONDS + FREEFORM_SECONDS;
    log.line("");
    log.line(&format!(
        "{} steps, {}s each, then {}s free-form: about {}s total.",
        STEPS.len(),
        STEP_SECONDS,
        FREEFORM_SECONDS,
        total
    ));
    log.line("Press the named control when asked. If you do not have it, or it");
    log.line("does nothing, just wait: '(nothing)' is a useful result too.");
    log.line("There is no way to quit early; power off if you need to stop.");
    log.line("");

    for (n, name) in STEPS.iter().enumerate() {
        log.line(&format!("STEP {}/{}: {}", n + 1, STEPS.len(), name));
        let tally = capture(STEP_SECONDS, &mut pointers, &mut touches);
        for line in tally.summary() {
            log.line(&line);
        }
    }

    log.line("");
    log.line(&format!("FREE-FORM: {FREEFORM_SECONDS}s, press anything not covered above"));
    let tally = capture(FREEFORM_SECONDS, &mut pointers, &mut touches);
    for line in tally.summary() {
        log.line(&line);
    }

    log.line("");
    log.line("=== efiprobe finished; efiprobe.log is on the ESP ===");
    Status::SUCCESS
}
