//! The SMBIOS structure table, as bytes.
//!
//! What the machine says it *is*: manufacturer, model, board, BIOS version.
//! None of it is needed to repair a partition table, and all of it is the
//! first thing anybody helping asks for — "which Deck, which BIOS?" — which
//! is exactly the kind of question the diagnostic report exists to have
//! answered before it is asked.
//!
//! Only the byte format lives here, for the same reason
//! [`crate::bootopt`]'s does: finding the table means walking the firmware's
//! configuration table through a raw pointer, and that cannot be tested on a
//! host. Handed the bytes, everything below is ordinary slice work with a
//! synthetic table in the tests. `bootfixr::smbios` is the ten lines that
//! fetch them.
//!
//! Every length in here comes out of the table itself, so nothing is trusted:
//! a structure whose declared length runs past the end of the buffer ends the
//! walk rather than reading whatever follows.

use crate::diag::{field, field_value};
use crate::guid::Guid;
use crate::style::{dim, title, Line};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The header every structure starts with: type, length, handle.
const HEADER: usize = 4;

/// The last structure in a table.
const TYPE_END_OF_TABLE: u8 = 127;

/// A refusal to walk a table forever.
///
/// Real tables hold a few dozen structures. The bound exists because the
/// walk is driven by lengths read out of the table, and a firmware that
/// declares a zero-length structure would otherwise loop for ever — which,
/// on a machine with no watchdog left armed, means a rescue tool that hangs
/// instead of reporting.
const MAX_STRUCTURES: usize = 1024;

/// One SMBIOS structure: a fixed formatted area, then its strings.
pub struct Structure<'a> {
    pub kind: u8,
    pub handle: u16,
    /// The formatted area, including the four-byte header.
    formatted: &'a [u8],
    /// The string set: NUL-terminated strings, ended by an empty one.
    strings: &'a [u8],
}

impl Structure<'_> {
    /// A byte of the formatted area, or `None` if this structure is too
    /// short to have one there.
    ///
    /// Short structures are normal, not damage: the fields were added to
    /// the specification over time, and an older firmware simply declares a
    /// shorter length. Every read goes through here so that reporting an
    /// older machine cannot read past its own structure.
    pub fn byte(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }

    pub fn word(&self, offset: usize) -> Option<u16> {
        let b = self.formatted.get(offset..offset + 2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    /// The string the byte at `offset` refers to.
    ///
    /// String references are one-based indices into the set that follows
    /// the structure; zero means "not specified", which is a normal answer
    /// and not a missing field.
    pub fn string_at(&self, offset: usize) -> Option<String> {
        let index = self.byte(offset)?;
        if index == 0 {
            return None;
        }
        let text = self.strings.split(|b| *b == 0).nth(index as usize - 1)?;
        let text = printable(text);
        (!text.is_empty()).then_some(text)
    }

    /// Sixteen bytes read as a GUID.
    ///
    /// SMBIOS stores a UUID with its first three fields little-endian —
    /// the same mixed-endian layout a GPT uses — so [`Guid`]'s own
    /// rendering is the correct one and no byte-swapping belongs here.
    pub fn guid_at(&self, offset: usize) -> Option<Guid> {
        Guid::read_from(self.formatted, offset)
    }
}

/// Printable ASCII only, trimmed, or nothing at all.
///
/// Same rule as `bootfixr::diskinfo`: a byte outside printable ASCII means
/// the field was not what we thought it was, and a plausible-looking wrong
/// answer in a report somebody is about to act on is worse than no answer.
///
/// Note what is *not* used to record that: a substitution character. `?` is
/// itself printable ASCII, so a firmware whose SKU or version legitimately
/// contains one would have the whole string thrown away and reported as
/// "not specified" — silently dropping a real value out of a file whose
/// entire premise is that nothing was left out.
fn printable(bytes: &[u8]) -> String {
    let mut text = String::new();
    for b in bytes.iter().take_while(|b| **b != 0) {
        if !(0x20..0x7f).contains(b) {
            return String::new();
        }
        text.push(*b as char);
    }
    text.trim().to_string()
}

/// Walk the structure table.
///
/// Stops at the end-of-table structure, at the end of the buffer, or at the
/// first structure that does not fit in what is left — in every case
/// returning what was read so far rather than nothing. A table truncated by
/// a firmware bug still names the machine in its first structure.
pub fn structures(bytes: &[u8]) -> Vec<Structure<'_>> {
    let mut out = Vec::new();
    let mut at = 0usize;

    while out.len() < MAX_STRUCTURES {
        let Some(head) = bytes.get(at..at + HEADER) else { break };
        let length = head[1] as usize;
        if length < HEADER {
            break;
        }
        let Some(formatted) = bytes.get(at..at + length) else { break };

        // The string set runs to a double NUL. A structure with no strings
        // still gets both of them, so the terminator is the same search
        // either way.
        let rest = &bytes[at + length..];
        let end = match rest.windows(2).position(|w| w == [0, 0]) {
            Some(i) => i + 2,
            // No terminator: the table is truncated, and this structure's
            // strings cannot be trusted to end where they should.
            None => break,
        };

        let kind = head[0];
        out.push(Structure {
            kind,
            handle: u16::from_le_bytes([head[2], head[3]]),
            formatted,
            strings: &rest[..end],
        });
        if kind == TYPE_END_OF_TABLE {
            break;
        }
        at += length + end;
    }
    out
}

/// How much of a masked value is left showing.
///
/// Enough to tell two machines apart, not enough to be one of them. A
/// vendor prefix is shared by every unit of a model, so it gives a reader
/// nothing the `product` line above it did not already say.
const KEPT: usize = 3;

/// The shortest value worth keeping a stub of.
///
/// Below this, three characters is most of the value rather than a hint at
/// it, so nothing is kept.
const MIN_TO_STUB: usize = 6;

/// A serial number, as it goes into a file somebody is about to publish.
///
/// Masked rather than dropped, and the length preserved, because the report
/// is also read by the machine's owner: "the field is there, it is twelve
/// characters, it starts FMT" is enough to match two reports to each other
/// and to check the firmware has not lost it, while a stranger reading the
/// thread cannot quote the number back to a warranty desk.
///
/// This is not privacy, and nothing here should be described as though it
/// were: a report still carries partition GUIDs, and a `UsbWwid()` device
/// path node *is* a serial number by definition. It is the one identifier
/// with a fixed offset, a known meaning, and a use to somebody
/// impersonating the owner — so it is the one worth masking.
pub fn mask(value: &str) -> String {
    let len = value.chars().count();
    let kept: String =
        if len >= MIN_TO_STUB { value.chars().take(KEPT).collect() } else { String::new() };
    let stars = "*".repeat(len - kept.chars().count());
    format!("{kept}{stars} (masked)")
}

/// Whether a serial-type field holds a real value worth masking, rather than
/// firmware's own way of saying the field was never set.
///
/// `Unknown` is what a lot of boards ship in the serial and asset-tag
/// strings when nobody has programmed one; masking it does not protect the
/// owner, it just makes an empty field look like a real serial to whoever
/// reads the report.
fn is_placeholder(value: &str) -> bool {
    value.eq_ignore_ascii_case("unknown")
}

/// The same, for a UUID: the first group stands, the rest goes.
///
/// The shape is kept so that it still reads as a UUID and not as a field
/// this tool failed to parse. The first group alone is 32 bits — plenty to
/// correlate two reports, nowhere near enough to reconstruct the value.
fn mask_guid(guid: Guid) -> String {
    let text = guid.to_string();
    let mut out = String::new();
    for (i, part) in text.split('-').enumerate() {
        if i > 0 {
            out.push('-');
        }
        if i == 0 {
            out.push_str(part);
        } else {
            out.push_str(&"*".repeat(part.len()));
        }
    }
    out
}

/// Everything worth putting in a report, from a structure table.
pub fn render(bytes: &[u8]) -> Vec<Line> {
    let all = structures(bytes);
    if all.is_empty() {
        return alloc::vec![dim("  The SMBIOS table holds no readable structures.")];
    }

    // Types 0, 1, 2 and 3 occur once and describe the machine; type 4
    // occurs once per socket, and the first one is the one worth reporting.
    let mut out = Vec::new();
    let find = |kind: u8| all.iter().find(|s| s.kind == kind);
    let secret =
        |value: Option<String>| value.map(|v| if is_placeholder(&v) { v } else { mask(&v) });

    if let Some(s) = find(1) {
        out.push(title("  System:"));
        out.push(field_value("manufacturer", s.string_at(0x04)));
        out.push(field_value("product", s.string_at(0x05)));
        out.push(field_value("version", s.string_at(0x06)));
        out.push(field_value("serial", secret(s.string_at(0x07))));
        out.push(field_value("UUID", s.guid_at(0x08).map(mask_guid)));
        out.push(field_value("SKU", s.string_at(0x19)));
        out.push(field_value("family", s.string_at(0x1A)));
    }

    if let Some(s) = find(0) {
        out.push(Line::blank());
        out.push(title("  BIOS:"));
        out.push(field_value("vendor", s.string_at(0x04)));
        out.push(field_value("version", s.string_at(0x05)));
        out.push(field_value("release date", s.string_at(0x08)));
        // 0xFF is the specification's "this firmware does not report a
        // release", and printing it as 255.255 — or the 0.0 that firmware
        // which simply left the field alone produces — is worse than saying
        // nothing.
        if let (Some(major), Some(minor)) = (s.byte(0x14), s.byte(0x15)) {
            if major != 0xFF && (major, minor) != (0, 0) {
                out.push(field("release", format!("{major}.{minor}")));
            }
        }
    }

    if let Some(s) = find(2) {
        out.push(Line::blank());
        out.push(title("  Baseboard:"));
        out.push(field_value("manufacturer", s.string_at(0x04)));
        out.push(field_value("product", s.string_at(0x05)));
        out.push(field_value("version", s.string_at(0x06)));
        out.push(field_value("serial", secret(s.string_at(0x07))));
    }

    if let Some(s) = find(3) {
        out.push(Line::blank());
        out.push(title("  Chassis:"));
        out.push(field_value("manufacturer", s.string_at(0x04)));
        out.push(field_value("version", s.string_at(0x06)));
        out.push(field_value("serial", secret(s.string_at(0x07))));
        out.push(field_value("asset tag", secret(s.string_at(0x08))));
    }

    if let Some(s) = find(4) {
        out.push(Line::blank());
        out.push(title("  Processor:"));
        out.push(field_value("manufacturer", s.string_at(0x07)));
        out.push(field_value("version", s.string_at(0x10)));
        out.push(field_value("max speed", s.word(0x14).map(|mhz| format!("{mhz} MHz"))));
        out.push(field_value("cores", s.byte(0x23).map(|n| n.to_string())));
        out.push(field_value("serial", secret(s.string_at(0x20))));
        out.push(field_value("asset tag", secret(s.string_at(0x21))));
        out.push(field_value("part number", secret(s.string_at(0x22))));
    }

    out.push(Line::blank());
    out.push(dim(format!("  {} structures in the table.", all.len())));
    out.push(dim("  Serial numbers and the system UUID are masked; see docs/report.md."));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use crate::style::plain;
    use alloc::vec;

    /// Build one structure: header, formatted area, then its strings.
    fn structure(kind: u8, handle: u16, fields: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut out = vec![kind, (HEADER + fields.len()) as u8];
        out.extend_from_slice(&handle.to_le_bytes());
        out.extend_from_slice(fields);
        for s in strings {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        // The set is terminated by an empty string; one with no strings at
        // all still gets both NULs.
        out.push(0);
        out
    }

    /// A machine that names itself, the way a Deck's does.
    fn table() -> Vec<u8> {
        let mut out = Vec::new();
        // Type 1: manufacturer, product, version, serial at 04..07, then a
        // UUID, a wake-up byte, SKU and family.
        let mut system = vec![1u8, 2, 3, 4];
        system.extend_from_slice(&[0xAA; 16]);
        system.extend_from_slice(&[6, 5, 6]);
        out.extend(structure(1, 0x0100, &system, &["Valve", "Jupiter", "1", "SERIAL123", "SKU9"]));
        // 04 vendor, 05 version, 06..07 starting segment, 08 release date.
        out.extend(structure(
            0,
            0x0000,
            &[1, 2, 0, 0, 3],
            &["Valve Corp", "F7A0131", "03/26/2024"],
        ));
        out.extend(structure(127, 0x7F00, &[], &[]));
        out
    }

    #[test]
    fn the_machine_names_itself() {
        let text = plain(&render(&table()));
        for wanted in ["Valve", "Jupiter", "F7A0131", "03/26/2024", "SKU9"] {
            assert!(text.contains(wanted), "no {wanted:?} in:\n{text}");
        }
    }

    /// The serial is the one field a stranger reading the thread could use
    /// against the owner, so it must not reach the file whole.
    #[test]
    fn the_serial_number_never_appears_in_full() {
        let text = plain(&render(&table()));
        assert!(!text.contains("SERIAL123"), "the serial was published:\n{text}");
        assert!(text.contains("SER****** (masked)"), "{text}");
        // Masked, not dropped: the owner still has to be able to match two
        // reports of their own machine to each other.
        assert!(text.contains("serial"), "{text}");
    }

    /// `Unknown` is firmware's own placeholder for an unset serial, not a
    /// real one, so it is left as-is rather than turned into stars.
    #[test]
    fn an_unset_serial_is_not_masked() {
        let mut system = vec![1u8, 2, 3, 4];
        system.extend_from_slice(&[0xAA; 16]);
        system.extend_from_slice(&[6, 5, 6]);
        let table = structure(1, 0x0100, &system, &["Valve", "Jupiter", "1", "Unknown", "SKU9"]);
        let text = plain(&render(&table));
        assert!(text.contains("serial            : Unknown"), "{text}");
        assert!(!text.contains("(masked)"), "{text}");
    }

    /// A stub of a short value is most of the value. Below the threshold,
    /// nothing is kept.
    #[test]
    fn masking_keeps_the_length_and_only_a_hint_of_the_value() {
        assert_eq!(mask("FMTBBK00A123"), "FMT********* (masked)");
        assert_eq!(mask("ABC123"), "ABC*** (masked)");
        assert_eq!(mask("ABCDE"), "***** (masked)");
        assert_eq!(mask("X"), "* (masked)");
        assert_eq!(mask(""), " (masked)");
    }

    /// The UUID keeps its shape so it still reads as one, and its first
    /// group so two reports can be matched.
    #[test]
    fn the_uuid_keeps_only_its_first_group() {
        let text = plain(&render(&table()));
        assert!(text.contains("AAAAAAAA-****-****-****-************"), "{text}");
        assert!(!text.contains("AAAAAAAA-AAAA"), "the UUID was published:\n{text}");
    }

    /// A UUID is stored the way a GPT stores a GUID, so it must come out
    /// looking like one and not byte-swapped.
    /// A question mark is printable ASCII, and a string holding one is a
    /// value to report rather than a field to discard.
    #[test]
    fn a_legitimate_question_mark_survives() {
        let bytes = structure(1, 0x0100, &[1, 2, 0, 0], &["Valve?", "Jupiter\u{7f}"]);
        let all = structures(&bytes);
        assert_eq!(all[0].string_at(0x04).as_deref(), Some("Valve?"));
        // Still nothing for a string with a byte outside printable ASCII,
        // which is the case the check exists for.
        assert_eq!(all[0].string_at(0x05), None);
    }

    /// A UUID is stored the way a GPT stores a GUID, so it must come out of
    /// the structure looking like one rather than byte-swapped. Asserted on
    /// the value itself, since the rendered line is masked.
    #[test]
    fn the_uuid_is_read_as_a_guid_and_not_byte_swapped() {
        let bytes = table();
        let all = structures(&bytes);
        let system = all.iter().find(|s| s.kind == 1).expect("a system structure");
        assert_eq!(
            system.guid_at(0x08).map(|g| g.to_string()).as_deref(),
            Some("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA")
        );
    }

    #[test]
    fn the_walk_stops_at_the_end_of_table_structure() {
        let mut bytes = table();
        // Anything after the type 127 structure must not be reached.
        bytes.extend(structure(1, 0x0101, &[1, 0, 0, 0], &["NOT THIS ONE"]));
        let all = structures(&bytes);
        assert_eq!(all.last().map(|s| s.kind), Some(TYPE_END_OF_TABLE));
        assert!(!plain(&render(&bytes)).contains("NOT THIS ONE"));
    }

    /// A field the firmware never wrote is "not specified", not a blank.
    #[test]
    fn an_unset_string_reference_says_so() {
        let bytes = structure(1, 0x0100, &[1, 0, 0, 0], &["Valve"]);
        let all = structures(&bytes);
        assert_eq!(all[0].string_at(0x04).as_deref(), Some("Valve"));
        assert_eq!(all[0].string_at(0x05), None);
        assert!(plain(&render(&bytes)).contains("not specified"));
    }

    /// An older firmware declares a shorter structure, and reading a field
    /// it does not have must answer nothing rather than the next
    /// structure's bytes.
    /// 0xFF means "not reported", and a version line reading 255.255 is a
    /// bug report about this tool rather than about the machine.
    #[test]
    fn a_bios_release_the_firmware_does_not_report_is_left_out() {
        let unreported = structure(
            0,
            0,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF],
            &["Valve Corp"],
        );
        assert!(!plain(&render(&unreported)).contains("255"));
        let zeroed = structure(
            0,
            0,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &["Valve Corp"],
        );
        assert!(!plain(&render(&zeroed)).contains("release           : 0.0"));
        let real = structure(
            0,
            0,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 15],
            &["Valve Corp"],
        );
        assert!(plain(&render(&real)).contains("1.15"));
    }

    #[test]
    fn a_short_structure_is_not_read_past() {
        let bytes = structure(1, 0x0100, &[1, 2], &["Valve", "Jupiter"]);
        let all = structures(&bytes);
        assert_eq!(all[0].string_at(0x05).as_deref(), Some("Jupiter"));
        assert_eq!(all[0].string_at(0x07), None);
        assert_eq!(all[0].guid_at(0x08), None);
    }

    /// Truncation is the case that must not read past the buffer, since the
    /// lengths driving the walk come out of the buffer itself.
    #[test]
    fn a_truncated_table_yields_what_was_readable() {
        let full = table();
        for cut in 1..full.len() {
            let all = structures(&full[..cut]);
            assert!(all.len() <= 3, "{cut}");
        }
    }

    #[test]
    fn a_zero_length_structure_does_not_loop_for_ever() {
        // Length 0 is impossible in a real table and is exactly what would
        // make a naive walk stand still.
        let bytes = alloc::vec![1u8, 0, 0, 0, 0, 0];
        assert!(structures(&bytes).is_empty());
    }

    #[test]
    fn nothing_at_all_is_reported_rather_than_panicked_on() {
        assert!(structures(&[]).is_empty());
        assert!(!plain(&render(&[])).is_empty());
    }
}
