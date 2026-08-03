//! The `Boot####` and `BootOrder` variable formats.
//!
//! A UEFI boot entry is an `EFI_LOAD_OPTION` (spec §3.1.3): a fixed six-byte
//! head, a NUL-terminated UCS-2 description, a device path whose length the
//! head declares, and whatever the entry's creator left after it.
//!
//! Three of those four parts are variable-length and two of them are sized
//! by a number stored in the same buffer, which is the whole reason this is
//! a module rather than a struct cast. A `Boot####` truncated by a firmware
//! that ran out of variable store will still parse as far as its declared
//! `FilePathListLength`, and will then describe a device path made of
//! whatever came next. Every boundary here is checked against the buffer
//! actually supplied.
//!
//! The device path is kept as **opaque bytes**. `gptcore` has no business
//! knowing what a device path node is — rendering one needs the firmware's
//! own vocabulary of hardware, and the `uefi` crate already does it. The
//! application passes the rendered text back in when it wants a line drawn.
//! That boundary is what keeps this file testable on the host.

use crate::style::{dim, line, title, Line, Style};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The entry is a candidate for booting. Firmware skips one without it.
pub const LOAD_OPTION_ACTIVE: u32 = 0x0000_0001;
/// Reconnect all drivers after loading, before transferring control.
pub const LOAD_OPTION_FORCE_RECONNECT: u32 = 0x0000_0002;
/// Present, bootable, but not to be offered in the firmware's own menu.
pub const LOAD_OPTION_HIDDEN: u32 = 0x0000_0008;
/// Which category field the entry declares.
pub const LOAD_OPTION_CATEGORY_MASK: u32 = 0x0000_1f00;
/// An application, not an OS loader: shown separately, never auto-booted.
pub const LOAD_OPTION_CATEGORY_APP: u32 = 0x0000_0100;

/// Attributes, FilePathListLength, and the description's terminator: the
/// least a well-formed load option can be.
const MIN_LEN: usize = 4 + 2 + 2;

/// One boot entry, decoded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoadOption {
    pub attributes: u32,
    pub description: String,
    /// The FilePathList, verbatim. See the module docs: never parsed here.
    pub device_path: Vec<u8>,
    /// Anything after the device path. Passed to the loaded image as its
    /// options; some loaders put a kernel command line here.
    pub optional_data: Vec<u8>,
}

impl LoadOption {
    pub fn is_active(&self) -> bool {
        self.attributes & LOAD_OPTION_ACTIVE != 0
    }

    pub fn is_hidden(&self) -> bool {
        self.attributes & LOAD_OPTION_HIDDEN != 0
    }

    pub fn is_app(&self) -> bool {
        self.attributes & LOAD_OPTION_CATEGORY_MASK == LOAD_OPTION_CATEGORY_APP
    }

    /// How the entry should be styled in a list.
    ///
    /// An entry the firmware will not boot should not look like one it
    /// will, which is the only reason the flags are read at all here.
    pub fn style(&self) -> Style {
        if self.is_active() {
            Style::Normal
        } else {
            Style::Dim
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// Shorter than the fixed head plus an empty description.
    TooShort { len: usize },
    /// The description ran to the end of the buffer without a NUL.
    UnterminatedDescription,
    /// `FilePathListLength` names more bytes than the variable holds.
    PathOverruns { declared: usize, available: usize },
    /// A `BootOrder` whose length is not a whole number of `u16`.
    OddOrderLength { len: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DecodeError::TooShort { len } => {
                write!(f, "truncated: {len} bytes, at least {MIN_LEN} needed")
            }
            DecodeError::UnterminatedDescription => {
                write!(f, "the description has no terminator")
            }
            DecodeError::PathOverruns { declared, available } => write!(
                f,
                "declares a {declared}-byte device path but only {available} bytes follow"
            ),
            DecodeError::OddOrderLength { len } => {
                write!(f, "boot order is {len} bytes, which is not a whole number of entries")
            }
        }
    }
}

fn le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Decode a `Boot####` variable.
pub fn decode(bytes: &[u8]) -> Result<LoadOption, DecodeError> {
    if bytes.len() < MIN_LEN {
        return Err(DecodeError::TooShort { len: bytes.len() });
    }
    let attributes = le_u32(bytes, 0);
    let path_len = le_u16(bytes, 4) as usize;

    // The description is UCS-2, so it is scanned two bytes at a time from
    // offset 6. A trailing odd byte cannot be part of it and cannot be the
    // terminator, so it is simply not scanned; the terminator search fails
    // and the entry is rejected rather than half-read.
    let rest = &bytes[6..];
    let units = rest.len() / 2;
    let mut end = None;
    for i in 0..units {
        if le_u16(rest, i * 2) == 0 {
            end = Some(i);
            break;
        }
    }
    let Some(end) = end else {
        return Err(DecodeError::UnterminatedDescription);
    };

    let description = char::decode_utf16((0..end).map(|i| le_u16(rest, i * 2)))
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();

    // Past the description and its terminator.
    let after = 6 + (end + 1) * 2;
    let available = bytes.len() - after;
    if path_len > available {
        return Err(DecodeError::PathOverruns { declared: path_len, available });
    }

    Ok(LoadOption {
        attributes,
        description,
        device_path: bytes[after..after + path_len].to_vec(),
        optional_data: bytes[after + path_len..].to_vec(),
    })
}

/// Encode a `Boot####` variable.
///
/// The inverse of [`decode`] for anything [`decode`] produced. A
/// description containing a NUL would encode a variable that decodes to a
/// shorter one, so the description is cut at the first NUL here rather than
/// silently producing a file that does not round-trip.
pub fn encode(opt: &LoadOption) -> Vec<u8> {
    let mut out = Vec::with_capacity(MIN_LEN + opt.device_path.len() + opt.optional_data.len());
    out.extend_from_slice(&opt.attributes.to_le_bytes());
    // Truncating to u16 is not a silent loss: a device path that long
    // cannot be stored in a load option at all, and `set_variable` would
    // reject the result. Saturate so the length never wraps to something
    // plausible.
    let path_len = u16::try_from(opt.device_path.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&path_len.to_le_bytes());
    for unit in opt.description.encode_utf16().take_while(|&u| u != 0) {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&opt.device_path[..path_len as usize]);
    out.extend_from_slice(&opt.optional_data);
    out
}

/// Decode a `BootOrder` variable: a bare array of little-endian slots.
pub fn decode_order(bytes: &[u8]) -> Result<Vec<u16>, DecodeError> {
    // is_multiple_of() needs rustc 1.87; MSRV is 1.85, so keep the older
    // spelling and silence the newer clippy lint that flags it.
    #[allow(unknown_lints, clippy::manual_is_multiple_of)]
    if bytes.len() % 2 != 0 {
        return Err(DecodeError::OddOrderLength { len: bytes.len() });
    }
    Ok((0..bytes.len() / 2).map(|i| le_u16(bytes, i * 2)).collect())
}

/// Encode a `BootOrder` variable.
pub fn encode_order(order: &[u16]) -> Vec<u8> {
    order.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// `1` becomes `Boot0001`.
pub fn slot_name(slot: u16) -> String {
    format!("Boot{slot:04X}")
}

/// `Boot0001` becomes `1`, and anything else becomes `None`.
///
/// Strict on the way in as well as the way out. The variable store is
/// enumerated to find entries, so a lenient match here would sweep up
/// `BootOrder`, `BootNext` and `BootCurrent` — all of which begin `Boot`
/// and none of which is a load option.
pub fn parse_slot(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("Boot")?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

/// The lowest slot not in `taken`.
///
/// Deliberately unlike the snapshot naming in [`crate::backup::next_name`],
/// which counts up from the highest and never fills a gap. That rule exists
/// because reusing a *filename* would destroy a backup nobody can get back.
/// A freed boot slot holds nothing; firmware and `efibootmgr` both reuse the
/// lowest, and leaving holes would march entries towards `BootFFFF` for no
/// reason.
pub fn next_free_slot(taken: &[u16]) -> Option<u16> {
    (0..=u16::MAX).find(|s| !taken.contains(s))
}

/// One variable to be written, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VarWrite {
    pub name: String,
    pub data: Vec<u8>,
    /// What this write accomplishes, for the screen shown before it.
    pub what: String,
}

/// Write a new entry, then put it in the boot order.
///
/// **The order of these two writes is the point, not an implementation
/// detail.** It is the same rule the GPT side follows when it writes an
/// entry array before the header that points at it: a power cut between
/// the two must not leave a pointer to something that does not exist.
///
/// Interrupted after the first write, NVRAM holds an entry that is not in
/// the boot order — which the firmware ignores and the view screen shows
/// under "Present, but not in the boot order". Interrupted the other way
/// round, `BootOrder` would name a slot with nothing behind it: a position
/// the firmware silently skips, and one no screen can explain. One of
/// those states is legible and the other is not.
///
/// `first` puts the new entry at the head of the order rather than the
/// tail, making it the default in the same operation.
pub fn plan_register(slot: u16, opt: &LoadOption, order: &[u16], first: bool) -> Vec<VarWrite> {
    let mut next: Vec<u16> = order.iter().copied().filter(|s| *s != slot).collect();
    if first {
        next.insert(0, slot);
    } else {
        next.push(slot);
    }

    alloc::vec![
        VarWrite {
            name: slot_name(slot),
            data: encode(opt),
            what: format!("the new entry, \"{}\"", opt.description),
        },
        VarWrite {
            name: String::from("BootOrder"),
            data: encode_order(&next),
            what: format!(
                "the boot order, with {} {}",
                slot_name(slot),
                if first { "first" } else { "last" }
            ),
        },
    ]
}

/// Move an existing entry to the front of the boot order.
///
/// Only `BootOrder` is touched. No entry is rewritten, so this cannot
/// damage one: the worst case is an order the operator did not want, and
/// the same screen puts it back.
pub fn plan_set_default(slot: u16, order: &[u16]) -> Vec<VarWrite> {
    alloc::vec![VarWrite {
        name: String::from("BootOrder"),
        data: encode_order(&reorder(slot, order)),
        what: format!("the boot order, with {} first", slot_name(slot)),
    }]
}

/// `order` with `slot` moved to the front, keeping the rest as they were.
///
/// Also the preview the confirmation screen shows, so the bytes authorised
/// and the list read are computed once rather than derived twice.
pub fn reorder(slot: u16, order: &[u16]) -> Vec<u16> {
    let mut next = alloc::vec![slot];
    next.extend(order.iter().copied().filter(|s| *s != slot));
    next
}

/// Boot one entry once, without changing the order.
///
/// The firmware consumes `BootNext` as it uses it, so this undoes itself.
/// It is the safe way to try an entry that may not work: if it fails, the
/// next boot is the old default again with nothing to put back.
pub fn plan_boot_next(slot: u16) -> Vec<VarWrite> {
    alloc::vec![VarWrite {
        name: String::from("BootNext"),
        data: slot.to_le_bytes().to_vec(),
        what: format!("a one-shot override to {}", slot_name(slot)),
    }]
}

/// The writes a plan will make, in the order it will make them.
pub fn render_plan(writes: &[VarWrite]) -> Vec<Line> {
    let mut out = alloc::vec![title("  Will write, in this order:")];
    for (i, w) in writes.iter().enumerate() {
        out.push(Line::new(
            format!("   {}. {}  ({} bytes)", i + 1, w.name, w.data.len()),
            Style::Key,
        ));
        out.push(dim(format!("      {}", w.what)));
    }
    out.push(Line::blank());
    out.push(dim("  No disk is touched. These are firmware variables;"));
    out.push(dim("  a reboot is what makes them take effect."));
    out
}

/// How the boot order reads before and after.
///
/// Two lists rather than "moved to the front", because the operator is
/// about to authorise the *result* and should be reading it, not reading a
/// description of the edit.
pub fn render_order_change(
    before: &[u16],
    after: &[u16],
    name: impl Fn(u16) -> String,
) -> Vec<Line> {
    let mut out = alloc::vec![dim("  Boot order now:")];
    for (i, s) in before.iter().enumerate() {
        out.push(dim(format!("   {}. {}", i + 1, name(*s))));
    }
    if before.is_empty() {
        out.push(dim("   (empty)"));
    }
    out.push(Line::blank());
    out.push(title("  Boot order after:"));
    for (i, s) in after.iter().enumerate() {
        let text = format!("   {}. {}", i + 1, name(*s));
        // Position one is the whole point of the change.
        out.push(if i == 0 { Line::new(text, Style::Key) } else { line(text) });
    }
    out
}

/// One row in a list of entries.
///
/// `*` marks an active entry, which is efibootmgr's notation. Anyone with a
/// boot problem has already been told to run `efibootmgr -v` by a forum
/// thread, so matching what that prints costs nothing and saves explaining.
pub fn summary(slot: u16, opt: &LoadOption) -> String {
    let mark = if opt.is_active() { '*' } else { ' ' };
    format!("{}{} {}", slot_name(slot), mark, opt.description)
}

/// The detail lines for one entry, below its device path.
///
/// The device path itself is not rendered here. It is the one part of an
/// entry `gptcore` cannot turn into text — see the module docs — and it is
/// long enough on real hardware to need wrapping to the console width,
/// which is knowledge this crate does not have either. The caller emits it
/// and then appends these.
pub fn render_flags(opt: &LoadOption) -> Vec<Line> {
    let mut out = Vec::new();
    let mut flags = Vec::new();
    if !opt.is_active() {
        flags.push("inactive");
    }
    if opt.is_hidden() {
        flags.push("hidden");
    }
    if opt.is_app() {
        flags.push("application");
    }
    if opt.attributes & LOAD_OPTION_FORCE_RECONNECT != 0 {
        flags.push("force-reconnect");
    }
    if !flags.is_empty() {
        // Not dim: "inactive" is the answer to "why will it not boot?", and
        // is the one thing on this screen worth noticing.
        out.push(line(format!("  [{}]", flags.join(", "))));
    }

    // Only worth a line when there is something in it. Most entries have
    // none, and a row of "options: 0 bytes" would bury the ones that do.
    if !opt.optional_data.is_empty() {
        out.push(dim(format!("  {} bytes of load options", opt.optional_data.len())));
    }
    out
}
