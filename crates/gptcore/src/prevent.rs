//! Preventing the recurrence, rather than repairing after the fact.
//!
//! The corruption observed on real hardware set `PartitionEntryLBA` to
//! 2016, which is `FirstUsableLBA - 32` on that disk, 32 being the size of
//! the entry array. In other words the writer placed the array immediately
//! below the first usable block instead of at LBA 2, where the spec pins
//! the main copy.
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
//! because the secondary GPT already holds it, whereas this modifies a
//! healthy
//! table on a theory. Callers must present it as such.

use crate::crc::Crc32;
use crate::mbr::MbrStatus;
use crate::repair::{Analysis, RepairPlan, Step};
use crate::style::{self, good, key, line, warn, Line, Style};
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Blocker {
    /// The disk carries a hybrid MBR.
    ///
    /// The same refusal `repair` makes, for the same reason: some legacy
    /// OS is relying on that view of the disk. It has to be repeated here
    /// because this operation reaches the headers by its own route —
    /// `analyze` decides the repair verdict, and nothing about a hybrid
    /// MBR makes the two GPTs *invalid*, so the checks below would happily
    /// pass a disk the tool has already said it will not touch.
    HybridMbr,
    /// One of the two tables is not currently valid. Repair first.
    TableNotHealthy,
    /// The main entry array is not at LBA 2, so the disk has a problem
    /// this operation is not the answer to.
    MainEntryLbaNotTwo { found: u64 },
    /// The declared entry geometry is unusable.
    GeometryUnknown,
    /// The two GPTs disagree about FirstUsableLBA already.
    HeadersDisagree { main: u64, secondary: u64 },
    /// A partition starts below where FirstUsableLBA would move to, so
    /// lowering it would describe a disk whose partitions precede the
    /// usable area.
    PartitionBelowProposed { index: usize, start: u64, proposed: u64 },
}

impl core::fmt::Display for Blocker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Blocker::HybridMbr => {
                write!(f, "this disk carries a hybrid MBR, which this tool never modifies")
            }
            Blocker::TableNotHealthy => {
                write!(f, "the GPT is not currently healthy; repair it first")
            }
            Blocker::MainEntryLbaNotTwo { found } => {
                write!(f, "main entry array is at LBA {found}, not 2")
            }
            Blocker::GeometryUnknown => write!(f, "the entry array geometry is unusable"),
            Blocker::HeadersDisagree { main, secondary } => {
                write!(f, "main GPT says FirstUsableLBA {main}, secondary says {secondary}")
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
    // First, and deliberately not delegated to `analysis.verdict`: that
    // field answers "what would a repair do", and a disk can be perfectly
    // healthy from a repair's point of view while still being one this
    // tool has promised not to write to.
    if analysis.mbr == MbrStatus::Hybrid {
        return Verdict::Refused(Blocker::HybridMbr);
    }
    let (Ok(main), Ok(secondary)) = (&analysis.main, &analysis.secondary) else {
        return Verdict::Refused(Blocker::TableNotHealthy);
    };
    if !main.is_valid() || !secondary.is_valid() {
        return Verdict::Refused(Blocker::TableNotHealthy);
    }
    if main.header.partition_entry_lba != 2 {
        return Verdict::Refused(Blocker::MainEntryLbaNotTwo {
            found: main.header.partition_entry_lba,
        });
    }
    let Some(blocks) = main.header.entry_array_blocks(analysis.block_size) else {
        return Verdict::Refused(Blocker::GeometryUnknown);
    };

    // The entry array occupies LBA 2..=(1 + blocks), so the first block
    // that can legitimately be used is the one after it.
    let proposed = 2 + blocks;
    let current = main.header.first_usable_lba;

    if main.header.first_usable_lba != secondary.header.first_usable_lba {
        return Verdict::Refused(Blocker::HeadersDisagree {
            main: main.header.first_usable_lba,
            secondary: secondary.header.first_usable_lba,
        });
    }
    if current <= proposed {
        return Verdict::AlreadyMinimal { current };
    }
    for (i, e) in main.used_entries() {
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
/// and are left alone. The main GPT goes first because it is what firmware
/// and the kernel actually read: if power is cut between the two writes,
/// the authoritative copy is the updated one, and re-running the operation
/// simply finishes the job.
pub fn plan(analysis: &Analysis, crc: &impl Crc32) -> Option<RepairPlan> {
    let Verdict::Applicable { proposed, .. } = assess(analysis) else {
        return None;
    };
    let main = analysis.main.as_ref().ok()?;
    let secondary = analysis.secondary.as_ref().ok()?;

    let mut new_main = main.header;
    new_main.first_usable_lba = proposed;
    let mut new_secondary = secondary.header;
    new_secondary.first_usable_lba = proposed;

    let steps = alloc::vec![
        Step::Write {
            lba: 1,
            data: new_main.to_block(analysis.block_size, crc),
            what: String::from("main GPT header (FirstUsableLBA lowered)"),
        },
        Step::Flush { why: "commit main GPT header" },
        Step::Write {
            lba: analysis.last_block,
            data: new_secondary.to_block(analysis.block_size, crc),
            what: String::from("secondary GPT header (FirstUsableLBA lowered)"),
        },
        Step::Flush { why: "commit secondary GPT header" },
    ];

    Some(RepairPlan { steps, header: new_main, entries: main.entries.clone() })
}

/// Lines describing the proposal, for the operator.
pub fn describe(verdict: Verdict) -> Vec<Line> {
    let mut out = Vec::new();
    match verdict {
        Verdict::Applicable { current, proposed } => {
            out.push(key(alloc::format!(
                "  FirstUsableLBA would move from {current} to {proposed}."
            )));
            out.push(line("  No partition moves and no filesystem is touched;"));
            out.push(line("  only the two GPT header blocks are rewritten."));
            out.push(Line::blank());
            style::block(
                &mut out,
                Style::Dim,
                &[
                    "  WHY: the corruption seen on this hardware set the main",
                    "  GPT's entry array to FirstUsableLBA minus its own length,",
                    "  which is correct only when no gap exists. Closing the gap",
                    "  should make that arithmetic produce the right answer.",
                ],
            );
            out.push(Line::blank());
            style::block(
                &mut out,
                Style::Warn,
                &[
                    "  This is a THEORY that fits the observed numbers exactly,",
                    "  not a diagnosis. It is reversible: set the value back.",
                ],
            );
        }
        Verdict::AlreadyMinimal { current } => {
            out.push(good(alloc::format!(
                "  FirstUsableLBA is already {current}, immediately after the"
            )));
            out.push(good("  entry array. There is no gap to close."));
        }
        Verdict::Refused(b) => {
            out.push(warn(alloc::format!("  Not applicable: {b}")));
        }
    }
    out
}
