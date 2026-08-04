//! Reading the firmware's boot configuration out of NVRAM.
//!
//! Everything here is read-only. The byte formats live in
//! [`gptcore::bootopt`], where they can be tested on the host; this module
//! is only the part that needs runtime services.
//!
//! **Entries are found by enumerating the variable store, not by walking
//! `BootOrder`.** That is the whole point of the screen. A `Boot####` that
//! exists but has fallen out of `BootOrder` is invisible to the firmware's
//! own boot menu, and is one of the two ways a machine that was working
//! yesterday stops offering the OS today. Reading the order and following it
//! would reproduce exactly the blindness the operator came here to escape.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use gptcore::bootopt::{self, LoadOption};
use uefi::runtime::{self, VariableAttributes, VariableVendor};
use uefi::{cstr16, CStr16, Status};

/// What NVRAM says about booting, as far as it could be read.
pub struct BootState {
    /// The entry the firmware used for this boot.
    pub current: Option<u16>,
    /// A one-shot override for the next boot, if one is pending.
    pub next: Option<u16>,
    /// The boot menu timeout, in seconds.
    pub timeout: Option<u16>,
    /// The order the firmware tries entries in.
    pub order: Result<Vec<u16>, String>,
    /// Every `Boot####` in the store, by slot, lowest first.
    ///
    /// An entry that will not decode is carried as an `Err` rather than
    /// dropped, the same way [`crate::store::Saved`] carries an unreadable
    /// snapshot. A corrupt boot entry is a finding, not a non-event.
    pub entries: Vec<(u16, Result<LoadOption, String>)>,
    /// Why the enumeration stopped early, if it did.
    pub truncated: Option<String>,
}

impl BootState {
    /// Entries that exist but are not in `BootOrder`, lowest slot first.
    pub fn orphans(&self) -> Vec<u16> {
        let order = self.order.as_deref().unwrap_or(&[]);
        self.entries.iter().map(|(s, _)| *s).filter(|s| !order.contains(s)).collect()
    }

    pub fn get(&self, slot: u16) -> Option<&Result<LoadOption, String>> {
        self.entries.iter().find(|(s, _)| *s == slot).map(|(_, e)| e)
    }
}

fn err(what: &str, status: Status) -> String {
    format!("{what} ({status:?})")
}

/// Read a global variable's raw bytes.
///
/// `Ok(None)` is "not present", which for most of these is a normal state —
/// `BootNext` is absent on nearly every boot. Every other failure is an
/// error and is reported as one.
fn get(name: &CStr16) -> Result<Option<Vec<u8>>, String> {
    match runtime::get_variable_boxed(name, &VariableVendor::GLOBAL_VARIABLE) {
        Ok((data, _)) => Ok(Some(data.into_vec())),
        Err(e) if e.status() == Status::NOT_FOUND => Ok(None),
        Err(e) => Err(err("cannot read", e.status())),
    }
}

/// The size of any variable, in any namespace, without reading it.
///
/// `GetVariable` with a zero-length buffer answers `BUFFER_TOO_SMALL` and
/// reports the size it wanted in the error payload. That is the documented
/// way to ask, and the only one that does not allocate — which matters for
/// the two callers that ask it: the diagnostic report walks the whole store,
/// and `dbx` alone can be tens of kilobytes of revocation hashes that mean
/// nothing to anybody diagnosing a boot failure.
///
/// `Ok(None)` is "not present".
pub fn size_of(name: &CStr16, vendor: &VariableVendor) -> Result<Option<usize>, String> {
    match runtime::get_variable(name, vendor, &mut []) {
        // An empty variable does not exist — setting one with no data
        // deletes it — so this branch should be unreachable. It is answered
        // honestly rather than treated as an error.
        Ok((data, _)) => Ok(Some(data.len())),
        Err(e) if e.status() == Status::BUFFER_TOO_SMALL => Ok(e.data().or(Some(0))),
        Err(e) if e.status() == Status::NOT_FOUND => Ok(None),
        Err(e) => Err(err("cannot read", e.status())),
    }
}

/// A `u16` variable: `BootCurrent`, `BootNext`, `Timeout`.
///
/// A variable of the wrong size is treated as absent rather than guessed
/// at. These three are decoration on the screen; refusing to show the whole
/// page because the firmware stored a four-byte `Timeout` would be the
/// wrong trade.
fn get_u16(name: &CStr16) -> Option<u16> {
    let data = get(name).ok()??;
    let bytes: [u8; 2] = data.get(..2)?.try_into().ok()?;
    (data.len() == 2).then(|| u16::from_le_bytes(bytes))
}

/// Every `Boot####` in the store, and why enumeration stopped if it did.
///
/// Two passes: collect the slot numbers first, then fetch each one.
/// `get_next_variable_key` walks firmware-owned iteration state, and
/// reading a variable in the middle of that walk is not something every
/// implementation is happy about.
fn enumerate() -> (Vec<u16>, Option<String>) {
    let mut slots = Vec::new();
    let mut truncated = None;

    for key in runtime::variable_keys() {
        let key = match key {
            Ok(key) => key,
            Err(e) => {
                truncated =
                    Some(err("the variable store could not be walked to the end", e.status()));
                break;
            }
        };
        if key.vendor != VariableVendor::GLOBAL_VARIABLE {
            continue;
        }
        if let Some(slot) = bootopt::parse_slot(&key.name.to_string()) {
            slots.push(slot);
        }
    }

    slots.sort_unstable();
    slots.dedup();
    (slots, truncated)
}

/// The settings worth saving alongside the entries.
///
/// `BootCurrent` is deliberately absent: it is volatile and set by the
/// firmware to describe the boot in progress, so a saved copy would be a
/// statement about a boot that already happened.
const SETTINGS: &[&str] = &["BootOrder", "BootNext", "Timeout"];

/// Every variable the boot process depends on, verbatim.
///
/// Raw bytes, not decoded entries: this feeds a snapshot whose whole
/// purpose is to survive a `Boot####` this build cannot parse. Variables
/// that are absent are skipped, and one that cannot be read is skipped
/// too, with its name returned so the caller can say the copy is partial
/// rather than quietly presenting it as complete.
pub fn capture() -> (Vec<(String, Vec<u8>)>, Vec<String>) {
    let (slots, truncated) = enumerate();
    let mut vars = Vec::new();
    let mut missed: Vec<String> = truncated.into_iter().collect();

    for name in SETTINGS {
        match uefi::CString16::try_from(*name).ok().map(|n| get(&n)) {
            Some(Ok(Some(data))) => vars.push((String::from(*name), data)),
            Some(Ok(None)) => {}
            _ => missed.push(String::from(*name)),
        }
    }
    for slot in slots {
        let name = bootopt::slot_name(slot);
        match uefi::CString16::try_from(name.as_str()).ok().map(|n| get(&n)) {
            Some(Ok(Some(data))) => vars.push((name, data)),
            Some(Ok(None)) => {}
            _ => missed.push(name),
        }
    }
    (vars, missed)
}

/// How a variable this tool writes is stored.
///
/// Non-volatile or the change is forgotten at the next power cycle, which
/// would make the whole exercise pointless. Both access bits because that
/// is what the firmware's own boot manager sets, and a variable the
/// firmware cannot read during boot service is not a boot entry.
const ATTRS: VariableAttributes = VariableAttributes::NON_VOLATILE
    .union(VariableAttributes::BOOTSERVICE_ACCESS)
    .union(VariableAttributes::RUNTIME_ACCESS);

/// Turn a refusal into something the operator can act on.
///
/// These three statuses are not bugs and not hardware faults; they are the
/// firmware saying no for a reason worth naming, and "SET_VARIABLE failed
/// (WRITE_PROTECTED)" tells someone with a machine that will not boot
/// nothing at all.
fn write_error(name: &str, status: Status) -> String {
    match status {
        Status::WRITE_PROTECTED => format!(
            "{name} is locked by the firmware. Some vendors seal the boot \
             variables; look for a setting to unlock them."
        ),
        Status::SECURITY_VIOLATION => format!(
            "{name} was refused by Secure Boot policy. Boot variables cannot \
             be changed from here while the firmware is in user mode."
        ),
        Status::OUT_OF_RESOURCES => format!(
            "there is no room left in NVRAM for {name}. Delete some boot \
             entries from the firmware's own menu first."
        ),
        other => err(&format!("{name} could not be written"), other),
    }
}

/// Write one variable.
pub fn write(w: &bootopt::VarWrite) -> Result<(), String> {
    let name = uefi::CString16::try_from(w.name.as_str())
        .map_err(|_| format!("{} is not a usable variable name", w.name))?;
    runtime::set_variable(&name, &VariableVendor::GLOBAL_VARIABLE, ATTRS, &w.data)
        .map_err(|e| write_error(&w.name, e.status()))
}

/// Apply a plan in order, stopping at the first refusal.
///
/// The error names how many writes had already landed, because a plan that
/// stopped halfway leaves NVRAM in a state the operator needs described:
/// after one write of a registration the entry exists but nothing points
/// at it, which is recoverable and worth saying out loud.
pub fn apply(writes: &[bootopt::VarWrite]) -> Result<(), (usize, String)> {
    for (done, w) in writes.iter().enumerate() {
        write(w).map_err(|e| (done, e))?;
    }
    Ok(())
}

/// Read the whole boot configuration.
pub fn read() -> BootState {
    let (slots, truncated) = enumerate();

    let entries = slots
        .into_iter()
        .map(|slot| {
            let name = match uefi::CString16::try_from(bootopt::slot_name(slot).as_str()) {
                Ok(n) => n,
                // Unreachable: the name came from `slot_name`. Carried
                // rather than unwrapped, because a panic under firmware
                // takes the machine down with no way to read the message.
                Err(_) => return (slot, Err("unusable variable name".to_string())),
            };
            let decoded = match get(&name) {
                Ok(Some(data)) => bootopt::decode(&data).map_err(|e| e.to_string()),
                // Present during the walk, gone by the time we asked for
                // it. Report it rather than pretending it was never there.
                Ok(None) => Err("disappeared while being read".to_string()),
                Err(e) => Err(e),
            };
            (slot, decoded)
        })
        .collect();

    let order = match get(cstr16!("BootOrder")) {
        Ok(Some(data)) => bootopt::decode_order(&data).map_err(|e| e.to_string()),
        Ok(None) => Ok(Vec::new()),
        Err(e) => Err(e),
    };

    BootState {
        current: get_u16(cstr16!("BootCurrent")),
        next: get_u16(cstr16!("BootNext")),
        timeout: get_u16(cstr16!("Timeout")),
        order,
        entries,
        truncated,
    }
}
