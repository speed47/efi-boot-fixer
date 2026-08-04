//! The plain-text diagnostic report: its shape, its name, and the parts of
//! it that can be written without firmware.
//!
//! The report exists to be *pasted into a forum post*. That is a different
//! audience from every other screen in this tool: not the operator standing
//! in front of the machine, who can press View and look again, but somebody
//! reading a wall of text on another continent a day later, who cannot ask
//! a follow-up question cheaply. So this renders far more than a screen
//! would — every header field, every partition's GUIDs, the four MBR
//! records as numbers — and it never abbreviates a value to fit.
//!
//! Two rules follow from that audience and are worth stating, because both
//! look like bugs from the point of view of the rest of the codebase:
//!
//! * **Nothing is wrapped to a width.** A device path is the single most
//!   useful line in a report about a machine that will not boot, and one
//!   broken across three lines cannot be searched for or diffed. Screens
//!   wrap because a Deck cannot scroll sideways; a text file has no such
//!   problem, and [`WIDTH`] is only what the section rules are drawn to.
//! * **Lines still carry a [`Style`].** The file drops it — [`to_text`]
//!   keeps the text alone — but the same lines are shown on screen before
//!   they are saved, and a report whose findings were colourless there
//!   would be a worse screen than the ones this tool already has.
//!
//! The UEFI-side gathering lives in `bootfixr::diag`; everything here can
//! be rendered on a host from a disk image, which is the same reason
//! [`crate::report`] is in this crate.

use crate::entry::PartitionEntry;
use crate::layout;
use crate::mbr::{self, MbrRecord};
use crate::repair::{Analysis, TableView};
use crate::report::{human_size, render_analysis};
use crate::style::{bad, dim, key, line, title, Line, Style};
use crate::IoError;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// How wide the section rules are drawn.
///
/// 78 so that a line of rule plus a terminal's own margin still fits an
/// 80-column window, which is what most of the places this gets pasted
/// into still assume.
pub const WIDTH: usize = 78;

/// Reports are named `diag-001.txt`, `diag-002.txt`, ...
///
/// The same scheme as the snapshots, for the same reasons — see
/// [`crate::backup::next_name`] — with one difference that matters: the
/// suffix is `.txt` because the file is meant to be opened by whoever finds
/// it, on whatever machine they carry it to.
pub(crate) const NAME_PREFIX: &str = "diag-";
pub(crate) const NAME_SUFFIX: &str = ".txt";
pub const MAX_SEQUENCE: u32 = 999;

/// The number in `diag-NNN.txt`, case-insensitively: FAT may hand back
/// `DIAG-001.TXT`.
pub fn sequence_of(name: &str) -> Option<u32> {
    if name.len() != NAME_PREFIX.len() + 3 + NAME_SUFFIX.len() {
        return None;
    }
    let (prefix, rest) = name.split_at(NAME_PREFIX.len());
    if !prefix.eq_ignore_ascii_case(NAME_PREFIX) {
        return None;
    }
    let (digits, suffix) = rest.split_at(3);
    if !suffix.eq_ignore_ascii_case(NAME_SUFFIX) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The next name to write, given what is already there.
///
/// Counts up from the highest and never fills a gap, exactly as the
/// snapshots do. A report is cheaper to lose than a partition table, but
/// two different reports sharing a name is the same failure, and somebody
/// comparing "before" and "after" is the person most likely to be holding
/// both.
pub fn next_name(existing: &[String]) -> Option<String> {
    let highest = existing.iter().filter_map(|n| sequence_of(n)).max().unwrap_or(0);
    let next = highest + 1;
    (next <= MAX_SEQUENCE).then(|| format!("{NAME_PREFIX}{next:03}{NAME_SUFFIX}"))
}

/// The report as it goes into the file.
///
/// CRLF, and a trailing one. The destinations are FAT volumes that get
/// carried to whatever machine the operator has to hand, and a file whose
/// line endings only work on the platform that wrote it would fail at
/// precisely the step this feature exists for: opening it and pasting it
/// somewhere. Everything that reads LF also reads CRLF; the reverse is not
/// true of every editor that ships on Windows.
pub fn to_text(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        // Trailing spaces survive a copy-paste and show up as ragged
        // whitespace in a code block; a blank line is written as nothing.
        out.push_str(l.text.trim_end());
        out.push_str("\r\n");
    }
    out
}

/// A section heading and its rule, with a blank line above it.
pub fn section(name: &str) -> Vec<Line> {
    let dashes = WIDTH.saturating_sub(name.chars().count() + 5);
    alloc::vec![Line::blank(), title(format!("--- {name} {}", "-".repeat(dashes)))]
}

/// How wide a field label is padded to, so a column of them reads down.
///
/// Long enough for `first usable LBA`, which is the longest label in the
/// report and the reason this is 18 rather than the 14 the screens use.
const LABEL: usize = 18;

/// `label : value`, indented under whatever heading it belongs to.
pub fn field(label: &str, value: impl AsRef<str>) -> Line {
    field_as(Style::Normal, label, value)
}

/// The same, for something the reader is most likely looking for.
pub fn field_key(label: &str, value: impl AsRef<str>) -> Line {
    field_as(Style::Key, label, value)
}

/// The same, saying what it means: a finding gets [`Style::Bad`] here
/// exactly as it would on a screen, and a report shown before it is saved
/// then reads like every other screen in the tool.
pub fn field_as(style: Style, label: &str, value: impl AsRef<str>) -> Line {
    Line::new(format!("    {:<LABEL$}: {}", label, value.as_ref()), style)
}

/// A field the machine may simply not have filled in.
///
/// "not specified" rather than a blank, and dim rather than normal: an
/// empty value after a colon reads as a bug in the tool, and somebody
/// comparing two reports needs to see that the field was asked for and came
/// back empty. SMBIOS is full of these — a string reference of zero is a
/// legitimate answer, not a missing field.
pub fn field_value(label: &str, value: Option<String>) -> Line {
    match value {
        Some(v) => field(label, v),
        None => field_as(Style::Dim, label, "not specified"),
    }
}

// ------------------------------------------------------------------ a disk

/// Everything the tables on one disk say, in full.
///
/// The screen's own analysis first, so that a reader who knows this tool
/// sees the familiar verdict block, then the detail no screen has room
/// for.
pub fn render_disk(analysis: &Analysis) -> Vec<Line> {
    let mut out = render_analysis(analysis);
    out.extend(render_mbr(analysis));
    out.extend(render_header("Main GPT header", &analysis.main));
    out.extend(render_header("Secondary GPT header", &analysis.secondary));
    out.extend(render_partitions(analysis));
    out
}

/// The four MBR records as numbers.
///
/// Printed even when [`mbr::inspect`] is happy, because "protective" is a
/// judgement and these are the evidence for it. A disk that boots Windows
/// through a hybrid MBR is diagnosed here and nowhere else.
fn render_mbr(analysis: &Analysis) -> Vec<Line> {
    let mut out = alloc::vec![Line::blank(), title("  MBR partition records at LBA 0:")];
    let Some(records) = mbr::records(&analysis.mbr_raw) else {
        out.push(bad("    the block at LBA 0 is shorter than 512 bytes"));
        return out;
    };
    out.push(dim(format!(
        "    {:>2}  {:>4}  {:>4}  {:>12}  {:>12}  {:<10} {}",
        "#", "boot", "type", "start LBA", "blocks", "start CHS", "end CHS"
    )));
    for (i, r) in records.iter().enumerate() {
        if r.is_empty() {
            out.push(dim(format!("    {:>2}  (unused)", i + 1)));
            continue;
        }
        out.push(record_line(i, r));
    }
    out
}

fn record_line(i: usize, r: &MbrRecord) -> Line {
    let text = format!(
        "    {:>2}  0x{:02X}  0x{:02X}  {:>12}  {:>12}  {:<10} {}",
        i + 1,
        r.boot_indicator,
        r.os_type,
        r.starting_lba,
        r.size_in_lba,
        chs(r.start_chs),
        chs(r.end_chs)
    );
    // 0xEE is the protective record and is expected; anything else in an
    // MBR alongside it is the finding.
    if r.os_type == mbr::OS_TYPE_GPT_PROTECTIVE {
        line(text)
    } else {
        key(text)
    }
}

fn chs(bytes: [u8; 3]) -> String {
    format!("{:02X} {:02X} {:02X}", bytes[0], bytes[1], bytes[2])
}

/// Every field of one GPT header, whether or not it is wrong.
///
/// A header is fourteen numbers and the whole argument about whether a
/// table is sound is conducted in them. `render_analysis` names the
/// defects; this is what somebody checks them against.
fn render_header(label: &str, view: &Result<TableView, IoError>) -> Vec<Line> {
    let mut out = alloc::vec![Line::blank(), title(format!("  {label}:"))];
    let view = match view {
        Ok(v) => v,
        Err(e) => {
            out.push(bad(format!("    could not be read ({e})")));
            return out;
        }
    };
    let h = &view.header;
    let signature = if h.signature == crate::header::GPT_SIGNATURE {
        format!("{:#018x}  \"EFI PART\"", h.signature)
    } else {
        format!("{:#018x}  NOT \"EFI PART\"", h.signature)
    };
    out.push(field("signature", signature));
    out.push(field("revision", format!("{:#010x}", h.revision)));
    out.push(field("header size", format!("{} bytes", h.header_size)));
    out.push(field("header CRC32", format!("{:#010x}", h.header_crc32)));
    out.push(field("reserved", format!("{:#010x}", h.reserved)));
    out.push(field_key("my LBA", h.my_lba.to_string()));
    out.push(field_key("alternate LBA", h.alternate_lba.to_string()));
    out.push(field("first usable LBA", h.first_usable_lba.to_string()));
    out.push(field("last usable LBA", h.last_usable_lba.to_string()));
    out.push(field_key("disk GUID", h.disk_guid.to_string()));
    out.push(field("entry array LBA", h.partition_entry_lba.to_string()));
    out.push(field(
        "entry array",
        format!("{} entries x {} bytes", h.number_of_partition_entries, h.size_of_partition_entry),
    ));
    out.push(field("entry CRC32", format!("{:#010x}", h.partition_entry_array_crc32)));

    if view.defects.is_empty() && view.entries_error.is_none() {
        out.push(field("defects", "none"));
    }
    for d in &view.defects {
        out.push(field_as(Style::Bad, "defect", d.to_string()));
    }
    if let Some(e) = view.entries_error {
        out.push(field_as(Style::Bad, "defect", format!("entry array unreadable ({e})")));
    }
    out
}

/// The partitions, from whichever tables could be read.
///
/// The secondary's list is compared against the main's rather than printed
/// twice. On a healthy disk they are identical and a second copy is forty
/// lines nobody reads; when they differ, that difference is the single most
/// important thing in the report and deserves to be stated, not left to be
/// spotted.
fn render_partitions(analysis: &Analysis) -> Vec<Line> {
    let mut out = Vec::new();
    let main = analysis.main.as_ref().ok();
    let secondary = analysis.secondary.as_ref().ok();

    match main {
        Some(view) => {
            out.extend(entry_list("Partitions, as the main GPT has them", view, analysis));
        }
        None => {
            out.push(Line::blank());
            out.push(bad("  The main GPT could not be read, so it lists no partitions."));
        }
    }

    // A table whose entry array would not read has no list to compare, and
    // saying the two "differ" would blame the disk for an I/O failure — the
    // difference being reported would be between a table and a bad sector.
    let listed = |v: Option<&TableView>| v.is_some_and(|v| v.entries_error.is_none());
    match (main, secondary) {
        (_, None) => {
            out.push(Line::blank());
            out.push(bad("  The secondary GPT could not be read, so it lists no partitions."));
        }
        (m, Some(s)) if !listed(m) || !listed(Some(s)) => {
            out.extend(entry_list("Partitions, as the secondary GPT has them", s, analysis));
        }
        (Some(m), Some(s)) if same_partitions(&m.entries, &s.entries) => {
            out.push(Line::blank());
            out.push(line("  The secondary GPT lists exactly the same partitions."));
        }
        (_, Some(s)) => {
            out.push(Line::blank());
            out.push(key("  The secondary GPT lists DIFFERENT partitions:"));
            out.extend(entry_list("Partitions, as the secondary GPT has them", s, analysis));
        }
    }
    out
}

/// Whether two tables describe the same set of used partitions.
fn same_partitions(a: &[PartitionEntry], b: &[PartitionEntry]) -> bool {
    let used = |e: &[PartitionEntry]| e.iter().filter(|e| e.is_used()).copied().collect::<Vec<_>>();
    used(a) == used(b)
}

fn entry_list(caption: &str, view: &TableView, analysis: &Analysis) -> Vec<Line> {
    let mut out = alloc::vec![Line::blank(), title(format!("  {caption}:"))];
    // An array that would not read leaves `entries` empty, which is the
    // same shape as a wiped table and means something entirely different.
    // The header block above already names the defect; the list must not
    // then contradict it by reporting a disk with no partitions on it.
    if let Some(e) = view.entries_error {
        out.push(bad(format!("    unknown - the entry array could not be read ({e})")));
        return out;
    }
    if view.entries.iter().all(|e| !e.is_used()) {
        out.push(bad("    none - every entry in the array is unused"));
        return out;
    }
    for (i, e) in view.used_entries() {
        let size = e
            .block_count()
            .map(|b| human_size(b.saturating_mul(analysis.block_size as u64)))
            .unwrap_or_else(|| "invalid range".to_string());
        let blocks = e.block_count().map(|b| b.to_string()).unwrap_or_else(|| "?".to_string());
        out.push(key(format!("    {:>2}  \"{}\"", i + 1, e.name_string())));
        out.push(line(format!(
            "          LBA {} - {}   {} blocks   {}",
            e.starting_lba, e.ending_lba, blocks, size
        )));
        out.push(line(format!(
            "          type   {}  ({})",
            e.type_guid,
            layout::describe_type(&e.type_guid)
        )));
        out.push(dim(format!("          unique {}", e.unique_guid)));
        out.push(dim(format!(
            "          attrs  {:#018x}{}",
            e.attributes,
            describe_attributes(e.attributes)
        )));
    }
    out
}

/// The attribute bits the UEFI spec gives a meaning to.
///
/// Bits 48..63 are reserved to the partition *type*, not to the spec —
/// systemd's discoverable-partitions scheme and Microsoft's basic-data
/// flags both live there — so they are reported as a number and not
/// guessed at.
fn describe_attributes(attributes: u64) -> String {
    let mut names = Vec::new();
    if attributes & 1 != 0 {
        names.push("required");
    }
    if attributes & 2 != 0 {
        names.push("no-block-io");
    }
    if attributes & 4 != 0 {
        names.push("legacy-bios-bootable");
    }
    let type_specific = attributes >> 48;
    if names.is_empty() && type_specific == 0 {
        return String::new();
    }
    let mut out = format!("  [{}", names.join(", "));
    if type_specific != 0 {
        if !names.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&format!("type-specific bits {type_specific:#06x}"));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use crate::header::GptHeader;
    use crate::mbr::MbrStatus;
    use crate::repair::Verdict;
    use crate::style::plain;
    use crate::{Guid, IoError};
    use alloc::vec;

    fn header() -> GptHeader {
        GptHeader {
            signature: crate::header::GPT_SIGNATURE,
            revision: 0x0001_0000,
            header_size: 92,
            header_crc32: 0,
            reserved: 0,
            my_lba: 1,
            alternate_lba: 2047,
            first_usable_lba: 34,
            last_usable_lba: 2014,
            disk_guid: Guid::ZERO,
            partition_entry_lba: 2,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            partition_entry_array_crc32: 0,
        }
    }

    fn view(entries: Vec<PartitionEntry>, entries_error: Option<IoError>) -> TableView {
        TableView {
            header: header(),
            raw: Vec::new(),
            entries_raw: Vec::new(),
            entries,
            defects: Vec::new(),
            entries_error,
        }
    }

    fn used(name: &str) -> PartitionEntry {
        let mut e = PartitionEntry {
            type_guid: Guid::from_fields(1, 0, 0, [0; 8]),
            unique_guid: Guid::from_fields(2, 0, 0, [0; 8]),
            starting_lba: 2048,
            ending_lba: 4095,
            attributes: 0,
            name: [0u16; crate::entry::NAME_LEN],
        };
        for (i, c) in name.encode_utf16().enumerate() {
            e.name[i] = c;
        }
        e
    }

    fn analysis(
        main: Result<TableView, IoError>,
        secondary: Result<TableView, IoError>,
    ) -> Analysis {
        Analysis {
            block_size: 512,
            last_block: 2047,
            mbr_raw: alloc::vec![0u8; 512],
            mbr: MbrStatus::Protective,
            main,
            secondary,
            verdict: Verdict::Healthy,
            recognition: None,
            rejection: None,
        }
    }

    /// An entry array that would not read leaves `entries` empty, which is
    /// exactly the shape of a wiped table and means something else
    /// entirely. Reporting "no partitions" would contradict the defect the
    /// header block prints three lines above it, in a file whose whole
    /// audience is somebody who cannot ask which one to believe.
    #[test]
    fn an_unreadable_entry_array_is_not_reported_as_an_empty_table() {
        let a = analysis(
            Ok(view(Vec::new(), Some(IoError::DeviceError))),
            Ok(view(alloc::vec![used("esp")], None)),
        );
        let text = plain(&render_partitions(&a));
        assert!(text.contains("the entry array could not be read"), "{text}");
        assert!(!text.contains("every entry in the array is unused"), "{text}");
        // And it is not a disagreement between the two tables: nothing was
        // read to disagree with.
        assert!(!text.contains("DIFFERENT"), "{text}");
    }

    /// The comparison still has to work, or the fix above would have bought
    /// silence rather than accuracy.
    #[test]
    fn two_readable_tables_are_still_compared() {
        let same = analysis(
            Ok(view(alloc::vec![used("esp")], None)),
            Ok(view(alloc::vec![used("esp")], None)),
        );
        assert!(plain(&render_partitions(&same)).contains("exactly the same partitions"));

        let differs = analysis(
            Ok(view(alloc::vec![used("esp")], None)),
            Ok(view(alloc::vec![used("home")], None)),
        );
        assert!(plain(&render_partitions(&differs)).contains("DIFFERENT"));
    }

    #[test]
    fn names_count_up_and_never_fill_a_gap() {
        assert_eq!(next_name(&[]).unwrap(), "diag-001.txt");
        let taken = vec!["diag-001.txt".to_string(), "diag-003.txt".to_string()];
        assert_eq!(next_name(&taken).unwrap(), "diag-004.txt");
    }

    #[test]
    fn the_other_kinds_of_saved_file_are_not_reports() {
        assert_eq!(sequence_of("diag-007.txt"), Some(7));
        assert_eq!(sequence_of("DIAG-007.TXT"), Some(7));
        assert_eq!(sequence_of("gpt-007.bkp"), None);
        assert_eq!(sequence_of("boot-007.bkp"), None);
        assert_eq!(sequence_of("diag-7.txt"), None);
        assert_eq!(sequence_of("diag-007.txt.bak"), None);
    }

    #[test]
    fn the_space_runs_out_rather_than_wrapping_round() {
        let taken = vec![std::format!("diag-{MAX_SEQUENCE:03}.txt")];
        assert!(next_name(&taken).is_none());
    }

    #[test]
    fn the_file_is_crlf_and_carries_no_trailing_spaces() {
        let text = to_text(&[line("a  "), Line::blank(), line("b")]);
        assert_eq!(text, "a\r\n\r\nb\r\n");
    }

    #[test]
    fn attribute_bits_are_named_where_the_spec_names_them() {
        assert_eq!(describe_attributes(0), "");
        assert_eq!(describe_attributes(1), "  [required]");
        assert_eq!(describe_attributes(4), "  [legacy-bios-bootable]");
        // A SteamOS priority field, which is type-specific and must not be
        // presented as though this crate knew what it meant.
        assert_eq!(describe_attributes(0x0006_0000_0000_0000), "  [type-specific bits 0x0006]");
    }
}
