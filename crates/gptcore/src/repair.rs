//! Reading both tables, deciding what is wrong, and planning the fix.
//!
//! The output of [`plan`] is an ordered list of [`Step`]s. Nothing here
//! performs I/O for the repair itself, so the ordering guarantee — entry
//! array durable before any header points at it — is a data structure that
//! can be asserted on in a test, not a convention in a comment.

use crate::crc::Crc32;
use crate::disk::{read_lbas, BlockDevice, IoError};
use crate::entry::{parse_array, PartitionEntry};
use crate::header::{Defect, GptHeader};
use crate::layout::{self, Confidence, Recognition, StructuralIssue};
use crate::mbr::{self, MbrStatus};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// One table (header plus its entry array) as found on disk.
#[derive(Clone, Debug)]
pub struct TableView {
    pub header: GptHeader,
    /// The block the header was read from.
    pub raw: Vec<u8>,
    /// Entry array bytes, truncated to the header's declared length.
    pub entries_raw: Vec<u8>,
    pub entries: Vec<PartitionEntry>,
    pub defects: Vec<Defect>,
    /// Set if the entry array could not be read; the header may still be
    /// intact, but its array CRC could not be checked.
    pub entries_error: Option<IoError>,
}

impl TableView {
    pub fn is_valid(&self) -> bool {
        self.defects.is_empty() && self.entries_error.is_none()
    }

    /// Everything checks out except the alternate pointer.
    ///
    /// The one defect a header can carry while its entry array, CRCs and
    /// geometry are all still trustworthy — so it can still serve as a
    /// repair source, provided the repair also writes it back corrected.
    pub fn only_wrong_alternate(&self) -> bool {
        self.entries_error.is_none()
            && !self.defects.is_empty()
            && self.defects.iter().all(|d| matches!(d, Defect::AlternateLbaWrongForRole { .. }))
    }

    pub fn used_entries(&self) -> impl Iterator<Item = (usize, &PartitionEntry)> {
        self.entries.iter().enumerate().filter(|(_, e)| e.is_used())
    }
}

/// Read the header at `lba` and the entry array it points at.
pub(crate) fn read_table<D: BlockDevice + ?Sized>(
    dev: &mut D,
    lba: u64,
    crc: &impl Crc32,
) -> Result<TableView, IoError> {
    let block_size = dev.block_size();
    let last_block = dev.last_block();
    let raw = read_lbas(dev, lba, 1)?;
    let header = GptHeader::parse(&raw).ok_or(IoError::DeviceError)?;

    // Only follow the entry-array pointer if the declared geometry is
    // sane; otherwise a corrupt header would drive the read.
    let mut entries_raw = Vec::new();
    let mut entries_error = None;
    if let (Some(len), Some(blocks)) =
        (header.entry_array_len(), header.entry_array_blocks(block_size))
    {
        match read_lbas(dev, header.partition_entry_lba, blocks) {
            Ok(mut bytes) => {
                bytes.truncate(len);
                entries_raw = bytes;
            }
            Err(e) => entries_error = Some(e),
        }
    }

    let entries = parse_array(
        &entries_raw,
        header.number_of_partition_entries,
        header.size_of_partition_entry,
    );
    let defects = header.validate(
        &raw,
        (!entries_raw.is_empty()).then_some(entries_raw.as_slice()),
        lba,
        last_block,
        block_size,
        crc,
    );

    Ok(TableView { header, raw, entries_raw, entries, defects, entries_error })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Both tables valid, protective MBR correct. Exit without writing.
    Healthy,
    /// Both tables valid but the protective MBR needs regenerating.
    MbrOnly,
    /// Main GPT bad, secondary good and plausible. The case this tool
    /// exists for.
    MainRepairable,
    /// Main GPT good, secondary bad. Not what breaks booting, so out of
    /// scope here, but worth telling the operator about.
    SecondaryDegraded,
    /// Neither table is usable. A rescue USB is the only option left.
    Unrecoverable,
    /// A hybrid MBR is present. Some legacy OS depends on that view and
    /// we will not touch the disk.
    RefusedHybridMbr,
    /// The secondary GPT parses, but the table it describes fails sanity
    /// checks.
    RefusedImplausibleSecondary,
}

impl Verdict {
    pub fn will_write(&self) -> bool {
        matches!(self, Verdict::MbrOnly | Verdict::MainRepairable)
    }
}

/// Why the secondary GPT was rejected as a repair source.
#[derive(Clone, Debug)]
pub enum Implausible {
    Structure(Vec<StructuralIssue>),
    /// No ESP or no rootfs: this is not a bootable SteamOS table.
    /// Boxed to keep the `Err` variant of `assess` small.
    Unrecognized(Box<Recognition>),
    /// The entry array would not fit between LBA 2 and the first usable
    /// block, so writing it would overwrite the start of a partition.
    EntryArrayCollides {
        entry_blocks: u64,
        first_usable: u64,
    },
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub block_size: u32,
    pub last_block: u64,
    pub mbr_raw: Vec<u8>,
    pub mbr: MbrStatus,
    pub main: Result<TableView, IoError>,
    pub secondary: Result<TableView, IoError>,
    pub verdict: Verdict,
    /// Sanity results for whichever table would be the repair source.
    pub recognition: Option<Recognition>,
    pub rejection: Option<Implausible>,
}

impl Analysis {
    /// Whichever table is worth believing, for purposes that only want to
    /// look at the partitions — identifying the disk in a picker, say.
    /// Prefers the main GPT, falls back to the secondary, and settles for
    /// a damaged main rather than nothing.
    pub fn best_view(&self) -> Option<&TableView> {
        let main = self.main.as_ref().ok();
        let secondary = self.secondary.as_ref().ok();
        main.filter(|t| t.is_valid()).or(secondary.filter(|t| t.is_valid())).or(main).or(secondary)
    }

    /// The table a repair would be built from.
    pub fn source(&self) -> Option<&TableView> {
        match self.verdict {
            Verdict::MainRepairable => self.secondary.as_ref().ok(),
            _ => None,
        }
    }
}

/// Read the disk and decide what, if anything, to do.
pub fn analyze<D: BlockDevice + ?Sized>(
    dev: &mut D,
    crc: &impl Crc32,
) -> Result<Analysis, IoError> {
    let block_size = dev.block_size();
    let last_block = dev.last_block();

    let mbr_raw = read_lbas(dev, 0, 1)?;
    let mbr_status = mbr::inspect(&mbr_raw, last_block);

    let main = read_table(dev, 1, crc);
    let secondary = read_table(dev, last_block, crc);

    let main_ok = main.as_ref().is_ok_and(|t| t.is_valid());
    let secondary_ok = secondary.as_ref().is_ok_and(|t| t.is_valid());

    let mut recognition = None;
    let mut rejection = None;

    let verdict = if mbr_status == MbrStatus::Hybrid {
        Verdict::RefusedHybridMbr
    } else if main_ok && secondary_ok {
        if mbr_status.needs_repair() {
            Verdict::MbrOnly
        } else {
            Verdict::Healthy
        }
    } else if main_ok {
        Verdict::SecondaryDegraded
    } else if secondary_ok || secondary.as_ref().is_ok_and(|t| t.only_wrong_alternate()) {
        // A secondary whose sole defect is a wrong alternate pointer is
        // still a trustworthy source — its array CRC holds — and the plan
        // below rewrites it corrected alongside the rebuilt main.
        let src = secondary.as_ref().expect("the guard above implies Ok");
        match assess(src, block_size) {
            Ok(rec) => {
                recognition = Some(rec);
                Verdict::MainRepairable
            }
            Err(why) => {
                if let Implausible::Unrecognized(rec) = &why {
                    recognition = Some(rec.as_ref().clone());
                }
                rejection = Some(why);
                Verdict::RefusedImplausibleSecondary
            }
        }
    } else {
        Verdict::Unrecoverable
    };

    Ok(Analysis {
        block_size,
        last_block,
        mbr_raw,
        mbr: mbr_status,
        main,
        secondary,
        verdict,
        recognition,
        rejection,
    })
}

/// Decide whether a valid table is a plausible repair source.
fn assess(src: &TableView, block_size: u32) -> Result<Recognition, Implausible> {
    let issues = layout::check_structure(
        &src.entries,
        src.header.first_usable_lba,
        src.header.last_usable_lba,
    );
    if !issues.is_empty() {
        return Err(Implausible::Structure(issues));
    }

    // The main entry array lands at LBA 2. If it would run into the
    // first partition, writing it would destroy data.
    let entry_blocks = src.header.entry_array_blocks(block_size).unwrap_or(u64::MAX);
    if 2u64.saturating_add(entry_blocks) > src.header.first_usable_lba {
        return Err(Implausible::EntryArrayCollides {
            entry_blocks,
            first_usable: src.header.first_usable_lba,
        });
    }

    let rec = layout::recognize(&src.entries);
    if rec.confidence == Confidence::Unrecognized {
        return Err(Implausible::Unrecognized(Box::new(rec)));
    }
    Ok(rec)
}

/// One durable operation. Order within a [`RepairPlan`] is significant.
#[derive(Clone, Debug)]
pub enum Step {
    Write {
        lba: u64,
        data: Vec<u8>,
        what: String,
    },
    /// A barrier. The preceding writes must reach the medium before any
    /// that follow.
    Flush {
        why: &'static str,
    },
}

#[derive(Clone, Debug)]
pub struct RepairPlan {
    pub steps: Vec<Step>,
    /// The header that will be written to LBA 1.
    pub header: GptHeader,
    /// The table as it will stand afterwards.
    pub entries: Vec<PartitionEntry>,
}

impl RepairPlan {
    pub fn writes(&self) -> impl Iterator<Item = (u64, &String)> {
        self.steps.iter().filter_map(|s| match s {
            Step::Write { lba, what, .. } => Some((*lba, what)),
            Step::Flush { .. } => None,
        })
    }
}

/// Build the ordered repair for an analysis, or `None` if there is nothing
/// to do or the disk was refused.
pub fn plan(analysis: &Analysis, crc: &impl Crc32) -> Option<RepairPlan> {
    match analysis.verdict {
        Verdict::MbrOnly => Some(RepairPlan {
            steps: alloc::vec![
                Step::Write {
                    lba: 0,
                    data: mbr::generate(
                        Some(&analysis.mbr_raw),
                        analysis.block_size,
                        analysis.last_block,
                    ),
                    what: String::from("protective MBR"),
                },
                Step::Flush { why: "commit protective MBR" },
            ],
            header: analysis.main.as_ref().ok()?.header,
            entries: analysis.main.as_ref().ok()?.entries.clone(),
        }),

        Verdict::MainRepairable => {
            let src = analysis.secondary.as_ref().ok()?;
            let block_size = analysis.block_size;
            let entry_blocks = src.header.entry_array_blocks(block_size)?;

            // Pad the array out to a whole number of blocks. The CRC
            // covers only the declared length, not the padding.
            let padded_len = (entry_blocks * block_size as u64) as usize;
            let mut entries_raw = src.entries_raw.clone();
            let declared = src.header.entry_array_len()?;
            entries_raw.resize(padded_len, 0);

            // Rebuild the header field by field rather than copying the
            // secondary's block, so every LBA is one we chose.
            let header = GptHeader {
                signature: src.header.signature,
                revision: src.header.revision,
                header_size: src.header.header_size,
                header_crc32: 0, // recomputed by to_block
                reserved: 0,
                my_lba: 1,
                alternate_lba: analysis.last_block,
                first_usable_lba: src.header.first_usable_lba,
                last_usable_lba: src.header.last_usable_lba,
                disk_guid: src.header.disk_guid,
                partition_entry_lba: 2,
                number_of_partition_entries: src.header.number_of_partition_entries,
                size_of_partition_entry: src.header.size_of_partition_entry,
                partition_entry_array_crc32: crc.crc32(&entries_raw[..declared]),
            };

            let mut steps = alloc::vec![
                Step::Write {
                    lba: 2,
                    data: entries_raw,
                    what: alloc::format!("partition entry array ({} blocks)", entry_blocks),
                },
                // The header written below asserts this array is present
                // and has this CRC. It must be on the medium first, or a
                // power cut leaves a valid header over stale bytes.
                Step::Flush { why: "entry array must be durable before the header points at it" },
            ];

            if analysis.mbr.needs_repair() {
                steps.push(Step::Write {
                    lba: 0,
                    data: mbr::generate(Some(&analysis.mbr_raw), block_size, analysis.last_block),
                    what: String::from("protective MBR"),
                });
            }

            steps.push(Step::Write {
                lba: 1,
                data: header.to_block(block_size, crc),
                what: String::from("main GPT header"),
            });

            // A source that was itself carrying a wrong alternate pointer
            // must not survive the repair as-is: the disk would come back
            // "repaired" with a secondary still pointing somewhere that is
            // not the main header. Only the pointer fields change; the
            // array it describes is untouched and its CRC still holds.
            if src.only_wrong_alternate() {
                let secondary =
                    GptHeader { my_lba: analysis.last_block, alternate_lba: 1, ..src.header };
                steps.push(Step::Write {
                    lba: analysis.last_block,
                    data: secondary.to_block(block_size, crc),
                    what: String::from("secondary GPT header (alternate pointer corrected)"),
                });
            }
            steps.push(Step::Flush { why: "commit main GPT header" });

            Some(RepairPlan { steps, header, entries: src.entries.clone() })
        }

        _ => None,
    }
}

/// Execute a plan. Stops at the first error, leaving the disk in whatever
/// state the completed steps produced — which is why the ordering matters.
pub fn apply<D: BlockDevice + ?Sized>(dev: &mut D, plan: &RepairPlan) -> Result<(), IoError> {
    for step in &plan.steps {
        match step {
            Step::Write { lba, data, .. } => dev.write_blocks(*lba, data)?,
            Step::Flush { .. } => dev.flush()?,
        }
    }
    Ok(())
}
