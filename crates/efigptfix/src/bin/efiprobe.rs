//! Input probe: what does the hardware actually send in the UEFI
//! environment?
//!
//! A Steam Deck has no built-in keyboard, and Bluetooth is not available in
//! firmware, so the repair tool cannot ask anyone to type a confirmation
//! word. What it *can* use depends on how the firmware exposes the buttons,
//! sticks, trackpads and touchscreen, and that is not something to guess at.
//!
//! This binary enumerates the input protocols the firmware publishes, then
//! logs every event it can see:
//!
//!   * `EFI_SIMPLE_TEXT_INPUT_PROTOCOL` - scan codes and Unicode characters
//!   * `EFI_SIMPLE_POINTER_PROTOCOL`    - relative motion (trackpads, mice)
//!   * `EFI_ABSOLUTE_POINTER_PROTOCOL`  - touchscreen, with its coordinate
//!                                        range, which would allow a
//!                                        touch-target interface
//!
//! Everything is written to `efiprobe.log` on the ESP it was launched from,
//! flushed after every line, as well as to the screen. The screen scrolls
//! and cannot be copied off the device; the file can be read from Linux
//! afterwards. Power-cutting the Deck at any point keeps whatever was
//! logged up to that moment.
//!
//! It exits on its own after a fixed run time, because without a keyboard
//! there may be no way to ask it to stop.

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
    fn mode(&self) -> String {
        // SAFETY: `mode` is a pointer the firmware filled in when the
        // protocol was installed; it stays valid while the protocol is open.
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
        // SAFETY: `self.0` is a live protocol interface and `state` is a
        // local we own. get_state returns NOT_READY when nothing changed.
        let status = unsafe { (self.0.get_state)(&mut self.0, &mut state) };
        (status == Status::SUCCESS).then_some(state)
    }
}

const RUN_SECONDS: u64 = 180;
const TICK_US: usize = 10_000; // 10 ms
const TICKS_PER_SEC: u64 = 1_000_000 / TICK_US as u64;

/// Screen plus `efiprobe.log`, flushed every line so a power-off keeps it.
struct Log {
    file: Option<RegularFile>,
}

impl Log {
    fn open() -> Self {
        let file = (|| {
            let mut fs = boot::get_image_file_system(boot::image_handle()).ok()?;
            let mut root = fs.open_volume().ok()?;
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

fn scan_code_name(code: ScanCode) -> &'static str {
    match code {
        ScanCode::UP => "UP",
        ScanCode::DOWN => "DOWN",
        ScanCode::RIGHT => "RIGHT",
        ScanCode::LEFT => "LEFT",
        ScanCode::HOME => "HOME",
        ScanCode::END => "END",
        ScanCode::INSERT => "INSERT",
        ScanCode::DELETE => "DELETE",
        ScanCode::PAGE_UP => "PAGE_UP",
        ScanCode::PAGE_DOWN => "PAGE_DOWN",
        ScanCode::FUNCTION_1 => "F1",
        ScanCode::FUNCTION_2 => "F2",
        ScanCode::FUNCTION_3 => "F3",
        ScanCode::FUNCTION_4 => "F4",
        ScanCode::FUNCTION_5 => "F5",
        ScanCode::FUNCTION_6 => "F6",
        ScanCode::FUNCTION_7 => "F7",
        ScanCode::FUNCTION_8 => "F8",
        ScanCode::FUNCTION_9 => "F9",
        ScanCode::FUNCTION_10 => "F10",
        ScanCode::ESCAPE => "ESCAPE",
        _ => "?",
    }
}

fn printable(c: char) -> String {
    match c {
        '\r' => "CR".to_string(),
        '\n' => "LF".to_string(),
        '\t' => "TAB".to_string(),
        '\u{8}' => "BACKSPACE".to_string(),
        c if (c as u32) < 0x20 => format!("ctrl-{:#04x}", c as u32),
        c => format!("'{c}'"),
    }
}

fn open_all<P: uefi::proto::ProtocolPointer + ?Sized>() -> Vec<ScopedProtocol<P>> {
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&P::GUID)) else {
        return Vec::new();
    };
    handles.iter().filter_map(|h| boot::open_protocol_exclusive::<P>(*h).ok()).collect()
}

fn count<P: uefi::proto::ProtocolPointer + ?Sized>() -> usize {
    boot::locate_handle_buffer(SearchType::ByProtocol(&P::GUID)).map(|h| h.len()).unwrap_or(0)
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("uefi helpers");
    let mut log = Log::open();

    log.line("=== efiprobe: UEFI input inventory ===");
    log.line(&format!(
        "firmware: {} rev {:#x}, UEFI {}.{}",
        system::firmware_vendor(),
        system::firmware_revision(),
        system::uefi_revision().major(),
        system::uefi_revision().minor()
    ));
    if log.file.is_none() {
        log.line("WARNING: could not create efiprobe.log on the ESP; screen only.");
    } else {
        log.line("logging to efiprobe.log on the ESP this was launched from");
    }

    let n_pointer = count::<Pointer>();
    let n_touch = count::<AbsolutePointer>();
    log.line(&format!("SimplePointer handles   : {n_pointer}"));
    log.line(&format!("AbsolutePointer handles : {n_touch}"));

    let mut pointers = open_all::<Pointer>();
    let mut touches = open_all::<AbsolutePointer>();
    for (i, t) in touches.iter().enumerate() {
        log.line(&format!("  AbsolutePointer[{i}] mode: {}", t.mode()));
    }
    for (i, p) in pointers.iter().enumerate() {
        let m = p.mode();
        log.line(&format!(
            "  SimplePointer[{i}] resolution: x {} y {} z {} counts/mm, buttons L:{} R:{}",
            m.resolution[0], m.resolution[1], m.resolution[2], m.has_button[0], m.has_button[1]
        ));
    }

    log.line("");
    log.line("Press buttons one at a time, pausing between them.");
    log.line("Suggested order: A, B, X, Y, D-pad up/down/left/right,");
    log.line("L1, R1, L2, R2, View, Menu, Steam, QAM, L4/L5/R4/R5,");
    log.line("then both sticks, both trackpads, then touch the screen.");
    log.line(&format!("Exits automatically after {RUN_SECONDS} seconds."));
    log.line("");

    let mut ticks: u64 = 0;
    let mut last_announce = 0u64;
    let deadline = RUN_SECONDS * TICKS_PER_SEC;

    while ticks < deadline {
        let secs = ticks / TICKS_PER_SEC;
        let stamp = format!("[{:>5}.{}s]", secs, (ticks % TICKS_PER_SEC) / 10);

        // Keyboard-ish input: this is where gamepad buttons will appear if
        // the firmware maps them at all.
        if let Some(key) = system::with_stdin(|stdin| stdin.read_key().ok().flatten()) {
            match key {
                Key::Printable(c) => {
                    let ch = char::from(c);
                    log.line(&format!(
                        "{stamp} KEY   unicode={:#06x} {}",
                        u16::from(c),
                        printable(ch)
                    ));
                    if ch == '\u{1b}' {
                        break;
                    }
                }
                Key::Special(code) => {
                    log.line(&format!(
                        "{stamp} KEY   scan={:#06x} {}",
                        code.0,
                        scan_code_name(code)
                    ));
                    if code == ScanCode::ESCAPE {
                        log.line("ESCAPE seen, exiting early.");
                        break;
                    }
                }
            }
        }

        for (i, p) in pointers.iter_mut().enumerate() {
            if let Ok(Some(s)) = p.read_state() {
                log.line(&format!(
                    "{stamp} PTR{i}  dx={} dy={} dz={} left={} right={}",
                    s.relative_movement[0],
                    s.relative_movement[1],
                    s.relative_movement[2],
                    s.button[0],
                    s.button[1]
                ));
            }
        }

        for (i, t) in touches.iter_mut().enumerate() {
            if let Some(s) = t.poll() {
                log.line(&format!(
                    "{stamp} TOUCH{i} x={} y={} z={} buttons={:#x}",
                    s.current_x, s.current_y, s.current_z, s.active_buttons
                ));
            }
        }

        if secs >= last_announce + 15 {
            last_announce = secs;
            log.line(&format!("{stamp} -- still listening, {}s left --", RUN_SECONDS - secs));
        }

        boot::stall(TICK_US);
        ticks += 1;
    }

    log.line("=== efiprobe finished; efiprobe.log is on the ESP ===");
    Status::SUCCESS
}
