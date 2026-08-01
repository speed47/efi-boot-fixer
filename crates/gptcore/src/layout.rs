//! Does the recovered table actually look like this machine's disk?
//!
//! Two independent questions, kept separate on purpose:
//!
//! * [`check_structure`] — is the table internally coherent? Overlapping
//!   or out-of-range partitions mean the backup is garbage, and that is a
//!   hard refusal.
//! * [`recognize`] — does it look like a SteamOS install? A backup that
//!   parses perfectly but describes a layout from two reinstalls ago is
//!   exactly the failure this is meant to catch. It can only ever lower
//!   confidence and demand a closer look from the operator; it never
//!   authorises a write on its own.
//!
//! The Deck dual-boots, so Windows partitions are expected company and are
//! recognised rather than treated as suspicious.

use crate::entry::PartitionEntry;
use crate::guid::Guid;
use alloc::string::String;
use alloc::vec::Vec;

pub const EFI_SYSTEM_PARTITION: Guid = Guid::from_fields(
    0xC12A_7328,
    0xF81F,
    0x11D2,
    [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
);
pub const LINUX_FILESYSTEM: Guid = Guid::from_fields(
    0x0FC6_3DAF,
    0x8483,
    0x4772,
    [0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4],
);
pub const LINUX_SWAP: Guid = Guid::from_fields(
    0x0657_FD6D,
    0xA4AB,
    0x43C4,
    [0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F],
);
pub const MS_BASIC_DATA: Guid = Guid::from_fields(
    0xEBD0_A0A2,
    0xB9E5,
    0x4433,
    [0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7],
);
pub const MS_RESERVED: Guid = Guid::from_fields(
    0xE3C9_E316,
    0x0B5C,
    0x4DB8,
    [0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15, 0xAE],
);
pub const WINDOWS_RECOVERY: Guid = Guid::from_fields(
    0xDE94_BBA4,
    0x06D1,
    0x4D40,
    [0xA1, 0x6A, 0xBF, 0xD5, 0x01, 0x79, 0xD6, 0xAC],
);

/// Partition type GUIDs that are unremarkable on a dual-booting Deck.
pub const KNOWN_TYPES: &[(Guid, &str)] = &[
    (EFI_SYSTEM_PARTITION, "EFI system partition"),
    (LINUX_FILESYSTEM, "Linux filesystem"),
    (LINUX_SWAP, "Linux swap"),
    (MS_BASIC_DATA, "Microsoft basic data"),
    (MS_RESERVED, "Microsoft reserved"),
    (WINDOWS_RECOVERY, "Windows recovery"),
];

pub fn describe_type(guid: &Guid) -> &'static str {
    KNOWN_TYPES.iter().find(|(g, _)| g == guid).map(|(_, name)| *name).unwrap_or("unknown")
}

/// The stock SteamOS 3.x A/B layout, by partition name and type.
///
/// Adjust this if your Deck's install differs; it is descriptive, not
/// normative, and only feeds the confidence estimate.
pub const STEAMOS_PARTITIONS: &[(&str, Guid)] = &[
    ("esp", EFI_SYSTEM_PARTITION),
    ("efi-A", LINUX_FILESYSTEM),
    ("efi-B", LINUX_FILESYSTEM),
    ("rootfs-A", LINUX_FILESYSTEM),
    ("rootfs-B", LINUX_FILESYSTEM),
    ("var-A", LINUX_FILESYSTEM),
    ("var-B", LINUX_FILESYSTEM),
    ("home", LINUX_FILESYSTEM),
];

/// Names whose absence means this is almost certainly not a bootable
/// SteamOS table, whatever else it contains.
pub const STEAMOS_CRITICAL: &[&str] = &["esp", "rootfs-A"];

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StructuralIssue {
    /// EndingLBA is before StartingLBA.
    InvertedRange { index: usize, start: u64, end: u64 },
    /// The partition falls outside the header's usable range.
    OutsideUsableRange { index: usize, start: u64, end: u64, first: u64, last: u64 },
    /// Two used partitions claim the same blocks.
    Overlap { a: usize, b: usize },
    /// No used entries at all.
    NoPartitions,
}

impl core::fmt::Display for StructuralIssue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StructuralIssue::InvertedRange { index, start, end } => {
                write!(f, "partition {} ends at {} before it starts at {}", index + 1, end, start)
            }
            StructuralIssue::OutsideUsableRange { index, start, end, first, last } => write!(
                f,
                "partition {} spans {}..={}, outside the usable range {}..={}",
                index + 1,
                start,
                end,
                first,
                last
            ),
            StructuralIssue::Overlap { a, b } => {
                write!(f, "partitions {} and {} overlap", a + 1, b + 1)
            }
            StructuralIssue::NoPartitions => write!(f, "the table contains no partitions"),
        }
    }
}

/// Hard coherence checks. A non-empty result must block any write.
pub fn check_structure(
    entries: &[PartitionEntry],
    first_usable: u64,
    last_usable: u64,
) -> Vec<StructuralIssue> {
    let mut issues = Vec::new();
    let used: Vec<(usize, &PartitionEntry)> =
        entries.iter().enumerate().filter(|(_, e)| e.is_used()).collect();

    if used.is_empty() {
        issues.push(StructuralIssue::NoPartitions);
        return issues;
    }

    for &(i, e) in &used {
        if e.ending_lba < e.starting_lba {
            issues.push(StructuralIssue::InvertedRange {
                index: i,
                start: e.starting_lba,
                end: e.ending_lba,
            });
            // An inverted range makes the overlap test meaningless.
            continue;
        }
        if e.starting_lba < first_usable || e.ending_lba > last_usable {
            issues.push(StructuralIssue::OutsideUsableRange {
                index: i,
                start: e.starting_lba,
                end: e.ending_lba,
                first: first_usable,
                last: last_usable,
            });
        }
    }

    for (pos, &(i, a)) in used.iter().enumerate() {
        if a.ending_lba < a.starting_lba {
            continue;
        }
        for &(j, b) in &used[pos + 1..] {
            if b.ending_lba >= b.starting_lba && a.overlaps(b) {
                issues.push(StructuralIssue::Overlap { a: i, b: j });
            }
        }
    }

    issues
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Confidence {
    /// Matches the stock SteamOS layout closely.
    SteamOs,
    /// Some SteamOS partitions present, but not the full set.
    Partial,
    /// Nothing recognisable. Could be a stale backup, or a disk this tool
    /// was never meant to touch.
    Unrecognized,
}

#[derive(Clone, Debug)]
pub struct Recognition {
    pub confidence: Confidence,
    pub matched: Vec<&'static str>,
    pub missing: Vec<&'static str>,
    /// Used partitions whose type GUID is not in [`KNOWN_TYPES`].
    pub unknown_types: Vec<(usize, Guid, String)>,
    /// Critical SteamOS partitions that are absent.
    pub missing_critical: Vec<&'static str>,
}

pub fn recognize(entries: &[PartitionEntry]) -> Recognition {
    let used: Vec<(usize, &PartitionEntry)> =
        entries.iter().enumerate().filter(|(_, e)| e.is_used()).collect();

    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for (name, type_guid) in STEAMOS_PARTITIONS {
        let found = used.iter().any(|(_, e)| e.name_string() == *name && e.type_guid == *type_guid);
        if found {
            matched.push(*name);
        } else {
            missing.push(*name);
        }
    }

    let unknown_types = used
        .iter()
        .filter(|(_, e)| !KNOWN_TYPES.iter().any(|(g, _)| *g == e.type_guid))
        .map(|(i, e)| (*i, e.type_guid, e.name_string()))
        .collect();

    let missing_critical =
        STEAMOS_CRITICAL.iter().copied().filter(|c| !matched.contains(c)).collect::<Vec<_>>();

    let confidence = if !missing_critical.is_empty() {
        // Without an ESP and a rootfs this cannot be the table we want,
        // however many other names happen to line up.
        Confidence::Unrecognized
    } else if matched.len() >= STEAMOS_PARTITIONS.len() - 2 {
        Confidence::SteamOs
    } else {
        Confidence::Partial
    };

    Recognition { confidence, matched, missing, unknown_types, missing_critical }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::NAME_LEN;
    extern crate std;

    fn entry(name: &str, type_guid: Guid, start: u64, end: u64) -> PartitionEntry {
        let mut n = [0u16; NAME_LEN];
        for (i, c) in name.encode_utf16().enumerate() {
            n[i] = c;
        }
        PartitionEntry {
            type_guid,
            unique_guid: Guid::from_fields(start as u32, 0, 0, [0; 8]),
            starting_lba: start,
            ending_lba: end,
            attributes: 0,
            name: n,
        }
    }

    fn steamos_table() -> Vec<PartitionEntry> {
        let mut v = Vec::new();
        let mut lba = 2048u64;
        for (name, guid) in STEAMOS_PARTITIONS {
            v.push(entry(name, *guid, lba, lba + 999));
            lba += 1000;
        }
        v
    }

    #[test]
    fn stock_layout_is_recognized() {
        let r = recognize(&steamos_table());
        assert_eq!(r.confidence, Confidence::SteamOs);
        assert!(r.missing.is_empty());
        assert!(r.missing_critical.is_empty());
    }

    #[test]
    fn windows_partitions_alongside_steamos_are_not_suspicious() {
        let mut table = steamos_table();
        table.push(entry("Basic data partition", MS_BASIC_DATA, 100_000, 200_000));
        table.push(entry("", WINDOWS_RECOVERY, 200_001, 210_000));
        let r = recognize(&table);
        assert_eq!(r.confidence, Confidence::SteamOs);
        assert!(r.unknown_types.is_empty(), "{:?}", r.unknown_types);
    }

    #[test]
    fn a_table_without_esp_or_rootfs_is_never_trusted() {
        let table = alloc::vec![
            entry("var-A", LINUX_FILESYSTEM, 2048, 3000),
            entry("var-B", LINUX_FILESYSTEM, 3001, 4000),
            entry("home", LINUX_FILESYSTEM, 4001, 5000),
        ];
        let r = recognize(&table);
        assert_eq!(r.confidence, Confidence::Unrecognized);
        assert_eq!(r.missing_critical, alloc::vec!["esp", "rootfs-A"]);
    }

    #[test]
    fn structure_of_stock_layout_is_clean() {
        assert!(check_structure(&steamos_table(), 2048, 100_000).is_empty());
    }

    #[test]
    fn overlap_is_a_structural_issue() {
        let mut table = steamos_table();
        table[3].starting_lba = table[2].starting_lba;
        let issues = check_structure(&table, 2048, 100_000);
        assert!(
            issues.iter().any(|i| matches!(i, StructuralIssue::Overlap { .. })),
            "{:?}",
            issues
        );
    }

    #[test]
    fn partition_past_the_end_of_the_disk_is_caught() {
        // The signature of a stale backup restored onto a smaller disk.
        let issues = check_structure(&steamos_table(), 2048, 5000);
        assert!(
            issues.iter().any(|i| matches!(i, StructuralIssue::OutsideUsableRange { .. })),
            "{:?}",
            issues
        );
    }

    #[test]
    fn inverted_range_does_not_also_report_a_spurious_overlap() {
        let table = alloc::vec![entry("esp", EFI_SYSTEM_PARTITION, 5000, 100)];
        let issues = check_structure(&table, 2048, 100_000);
        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0], StructuralIssue::InvertedRange { .. }));
    }

    #[test]
    fn empty_table_is_rejected() {
        assert_eq!(check_structure(&[], 2048, 100_000), alloc::vec![StructuralIssue::NoPartitions]);
    }
}
