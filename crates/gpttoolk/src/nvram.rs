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
use uefi::runtime::{self, VariableVendor};
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
    /// dropped, the same way [`crate::esp::Saved`] carries an unreadable
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
