//! Preventing the recurrence, rather than repairing after the fact.
//!
//! The corruption observed on real hardware set `PartitionEntryLBA` to
//! 2016, which is `FirstUsableLBA - 32` on that disk, 32 being the size of
//! the entry array. In other words the writer placed the array immediately
//! below the first usable block instead of at LBA 2, where the spec pins
//! the primary copy.
//!
//! On a conventionally formatted disk `FirstUsableLBA` is 34, and that same
//! arithmetic gives `34 - 32 = 2`, which is correct. It only goes wrong
//! where a gap exists between the end of the entry array and the first
//! usable block — as it does on a table written by util-linux fdisk.
//!
//! So: lowering `FirstUsableLBA` to sit immediately after the entry array
//! should make the faulty arithmetic produce the right answer. No partition
//! moves and no data is touched; only two header fields change.
//!
//! **This is a hypothesis that fits the arithmetic exactly, not a diagnosis
//! of the writer.** It is offered as a separate, separately-confirmed
//! operation for that reason: a repair restores a table that is known-good
//! because the backup already holds it, whereas this modifies a healthy
//! table on a theory. Callers must present it as such.

use crate::crc::Crc32;
use crate::repair::{Analysis, RepairPlan, Step};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocker {
    /// One of the two tables is not currently valid. Repair first.
    TableNotHealthy,
    /// The primary entry array is not at LBA 2, so the disk has a problem
    /// this operation is not the answer to.
    PrimaryEntryLbaNotTwo { found: u64 },
    /// The declared entry geometry is unusable.
    GeometryUnknown,
    /// Primary and backup disagree about FirstUsableLBA already.
    HeadersDisagree { primary: u64, backup: u64 },
    /// A partition starts below where FirstUsableLBA would move to, so
    /// lowering it would describe a disk whose partitions precede the
    /// usable area.
    PartitionBelowProposed { index: usize, start: u64, proposed: u64 },
}

impl core::fmt::Display for Blocker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Blocker::TableNotHealthy => {
                write!(f, "the GPT is not currently healthy; repair it first")
            }
            Blocker::PrimaryEntryLbaNotTwo { found } => {
                write!(f, "primary entry array is at LBA {found}, not 2")
            }
            Blocker::GeometryUnknown => write!(f, "the entry array geometry is unusable"),
            Blocker::HeadersDisagree { primary, backup } => {
                write!(f, "primary says FirstUsableLBA {primary}, backup says {backup}")
            }
            Blocker::PartitionBelowProposed { index, start, proposed } => write!(
                f,
                "partition {} starts at LBA {}, below the proposed first usable block {}",
                index + 1,
                start,
                proposed
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// FirstUsableLBA can be moved down from `current` to `proposed`.
    Applicable {
        current: u64,
        proposed: u64,
    },
    /// Already immediately after the entry array; nothing to do.
    AlreadyMinimal {
        current: u64,
    },
    Refused(Blocker),
}

impl Verdict {
    pub fn will_write(&self) -> bool {
        matches!(self, Verdict::Applicable { .. })
    }
}

/// Decide whether the gap can be closed on this disk.
pub fn assess(analysis: &Analysis) -> Verdict {
    let (Ok(primary), Ok(backup)) = (&analysis.primary, &analysis.backup) else {
        return Verdict::Refused(Blocker::TableNotHealthy);
    };
    if !primary.is_valid() || !backup.is_valid() {
        return Verdict::Refused(Blocker::TableNotHealthy);
    }
    if primary.header.partition_entry_lba != 2 {
        return Verdict::Refused(Blocker::PrimaryEntryLbaNotTwo {
            found: primary.header.partition_entry_lba,
        });
    }
    let Some(blocks) = primary.header.entry_array_blocks(analysis.block_size) else {
        return Verdict::Refused(Blocker::GeometryUnknown);
    };

    // The entry array occupies LBA 2..=(1 + blocks), so the first block
    // that can legitimately be used is the one after it.
    let proposed = 2 + blocks;
    let current = primary.header.first_usable_lba;

    if primary.header.first_usable_lba != backup.header.first_usable_lba {
        return Verdict::Refused(Blocker::HeadersDisagree {
            primary: primary.header.first_usable_lba,
            backup: backup.header.first_usable_lba,
        });
    }
    if current <= proposed {
        return Verdict::AlreadyMinimal { current };
    }
    for (i, e) in primary.used_entries() {
        if e.starting_lba < proposed {
            return Verdict::Refused(Blocker::PartitionBelowProposed {
                index: i,
                start: e.starting_lba,
                proposed,
            });
        }
    }
    Verdict::Applicable { current, proposed }
}

/// Rewrite both headers with the lowered `FirstUsableLBA`.
///
/// Only the two header blocks change; both entry arrays are already correct
/// and are left alone. The primary goes first because it is what firmware
/// and the kernel actually read: if power is cut between the two writes,
/// the authoritative copy is the updated one, and re-running the operation
/// simply finishes the job.
pub fn plan(analysis: &Analysis, crc: &impl Crc32) -> Option<RepairPlan> {
    let Verdict::Applicable { proposed, .. } = assess(analysis) else {
        return None;
    };
    let primary = analysis.primary.as_ref().ok()?;
    let backup = analysis.backup.as_ref().ok()?;

    let mut new_primary = primary.header;
    new_primary.first_usable_lba = proposed;
    let mut new_backup = backup.header;
    new_backup.first_usable_lba = proposed;

    let steps = alloc::vec![
        Step::Write {
            lba: 1,
            data: new_primary.to_block(analysis.block_size, crc),
            what: String::from("primary GPT header (FirstUsableLBA lowered)"),
        },
        Step::Flush { why: "commit primary header" },
        Step::Write {
            lba: analysis.last_block,
            data: new_backup.to_block(analysis.block_size, crc),
            what: String::from("backup GPT header (FirstUsableLBA lowered)"),
        },
        Step::Flush { why: "commit backup header" },
    ];

    Some(RepairPlan { steps, header: new_primary, entries: primary.entries.clone() })
}

/// Lines describing the proposal, for the operator.
pub fn describe(verdict: Verdict) -> Vec<String> {
    let mut out = Vec::new();
    match verdict {
        Verdict::Applicable { current, proposed } => {
            out.push(alloc::format!("  FirstUsableLBA would move from {current} to {proposed}."));
            out.push(String::from("  No partition moves and no filesystem is touched;"));
            out.push(String::from("  only the two GPT header blocks are rewritten."));
            out.push(String::new());
            out.push(String::from("  WHY: the corruption seen on this hardware set the primary"));
            out.push(String::from("  entry array to FirstUsableLBA minus its own length, which"));
            out.push(String::from("  is correct only when no gap exists. Closing the gap should"));
            out.push(String::from("  make that arithmetic produce the right answer."));
            out.push(String::new());
            out.push(String::from("  This is a THEORY that fits the observed numbers exactly,"));
            out.push(String::from("  not a diagnosis. It is reversible: set the value back."));
        }
        Verdict::AlreadyMinimal { current } => {
            out.push(alloc::format!(
                "  FirstUsableLBA is already {current}, immediately after the"
            ));
            out.push(String::from("  entry array. There is no gap to close."));
        }
        Verdict::Refused(b) => {
            out.push(alloc::format!("  Not applicable: {b}"));
        }
    }
    out
}
