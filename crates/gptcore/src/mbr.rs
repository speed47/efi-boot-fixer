//! The protective MBR at LBA 0.
//!
//! Regeneration deliberately preserves bytes 0..440. Those are boot code,
//! irrelevant to UEFI boot but not ours to discard, and a hybrid MBR is
//! refused outright rather than "repaired" into a protective one.

use alloc::vec::Vec;

pub const MBR_BOOT_SIGNATURE: u16 = 0xAA55;
pub const OS_TYPE_GPT_PROTECTIVE: u8 = 0xEE;

const OFF_BOOT_CODE_END: usize = 440;
const OFF_RECORDS: usize = 446;
const RECORD_SIZE: usize = 16;
const RECORD_COUNT: usize = 4;
const OFF_SIGNATURE: usize = 510;

/// Canonical start CHS for a protective record: head 0, sector 2, cyl 0.
const START_CHS: [u8; 3] = [0x00, 0x02, 0x00];
/// "Beyond CHS addressing", which is what every modern tool writes.
const END_CHS: [u8; 3] = [0xFF, 0xFF, 0xFF];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MbrRecord {
    pub boot_indicator: u8,
    pub start_chs: [u8; 3],
    pub os_type: u8,
    pub end_chs: [u8; 3],
    pub starting_lba: u32,
    pub size_in_lba: u32,
}

impl MbrRecord {
    fn parse(buf: &[u8]) -> Self {
        MbrRecord {
            boot_indicator: buf[0],
            start_chs: [buf[1], buf[2], buf[3]],
            os_type: buf[4],
            end_chs: [buf[5], buf[6], buf[7]],
            starting_lba: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            size_in_lba: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        }
    }

    fn write_to(&self, buf: &mut [u8]) {
        buf[0] = self.boot_indicator;
        buf[1..4].copy_from_slice(&self.start_chs);
        buf[4] = self.os_type;
        buf[5..8].copy_from_slice(&self.end_chs);
        buf[8..12].copy_from_slice(&self.starting_lba.to_le_bytes());
        buf[12..16].copy_from_slice(&self.size_in_lba.to_le_bytes());
    }

    fn is_empty(&self) -> bool {
        self.os_type == 0 && self.starting_lba == 0 && self.size_in_lba == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MbrStatus {
    /// Exactly one 0xEE record starting at LBA 1 and covering the disk.
    Protective,
    /// Protective in shape, but SizeInLBA does not cover the disk. This is
    /// what a resized or re-imaged disk leaves behind.
    WrongSize { found: u32, expected: u32 },
    /// A 0xEE record alongside real partition records. Some legacy OS is
    /// relying on this view; regenerating it would break that, so we never
    /// touch a disk in this state.
    Hybrid,
    /// No boot signature, or no protective record at all.
    Absent,
}

impl MbrStatus {
    /// Whether this MBR should be rewritten as part of a repair.
    pub fn needs_repair(&self) -> bool {
        matches!(self, MbrStatus::WrongSize { .. } | MbrStatus::Absent)
    }
}

/// What SizeInLBA should be: the disk size minus one, saturated.
pub fn expected_size_in_lba(last_block: u64) -> u32 {
    last_block.min(u32::MAX as u64) as u32
}

pub fn inspect(block: &[u8], last_block: u64) -> MbrStatus {
    if block.len() < 512 {
        return MbrStatus::Absent;
    }
    let signature = u16::from_le_bytes([block[OFF_SIGNATURE], block[OFF_SIGNATURE + 1]]);
    if signature != MBR_BOOT_SIGNATURE {
        return MbrStatus::Absent;
    }

    let records: Vec<MbrRecord> = (0..RECORD_COUNT)
        .map(|i| MbrRecord::parse(&block[OFF_RECORDS + i * RECORD_SIZE..]))
        .collect();

    let protective = records.iter().position(|r| r.os_type == OS_TYPE_GPT_PROTECTIVE);
    let Some(idx) = protective else {
        return MbrStatus::Absent;
    };

    // Any other populated record means somebody built a hybrid MBR.
    if records.iter().enumerate().any(|(i, r)| i != idx && !r.is_empty()) {
        return MbrStatus::Hybrid;
    }

    let expected = expected_size_in_lba(last_block);
    let record = &records[idx];
    if record.starting_lba != 1 || record.size_in_lba != expected {
        return MbrStatus::WrongSize { found: record.size_in_lba, expected };
    }
    MbrStatus::Protective
}

/// Build a protective MBR, carrying over boot code and disk signature from
/// `existing` when it is present.
pub fn generate(existing: Option<&[u8]>, block_size: u32, last_block: u64) -> Vec<u8> {
    let mut block = alloc::vec![0u8; block_size as usize];
    if let Some(old) = existing {
        let carry = OFF_BOOT_CODE_END.min(old.len());
        block[..carry].copy_from_slice(&old[..carry]);
    }
    // Records 2..4 stay zero; only the protective record is populated.
    MbrRecord {
        boot_indicator: 0x00,
        start_chs: START_CHS,
        os_type: OS_TYPE_GPT_PROTECTIVE,
        end_chs: END_CHS,
        starting_lba: 1,
        size_in_lba: expected_size_in_lba(last_block),
    }
    .write_to(&mut block[OFF_RECORDS..OFF_RECORDS + RECORD_SIZE]);
    block[OFF_SIGNATURE..OFF_SIGNATURE + 2].copy_from_slice(&MBR_BOOT_SIGNATURE.to_le_bytes());
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    const LAST: u64 = 1_000_000;

    #[test]
    fn generated_mbr_validates_as_protective() {
        let mbr = generate(None, 512, LAST);
        assert_eq!(inspect(&mbr, LAST), MbrStatus::Protective);
    }

    #[test]
    fn boot_code_survives_regeneration() {
        let mut old = alloc::vec![0u8; 512];
        old[..440].fill(0xAB);
        let mbr = generate(Some(&old), 512, LAST);
        assert!(mbr[..440].iter().all(|&b| b == 0xAB));
        assert_eq!(inspect(&mbr, LAST), MbrStatus::Protective);
    }

    #[test]
    fn zeroed_mbr_is_absent() {
        assert_eq!(inspect(&[0u8; 512], LAST), MbrStatus::Absent);
        assert!(inspect(&[0u8; 512], LAST).needs_repair());
    }

    #[test]
    fn size_that_does_not_cover_the_disk_is_flagged() {
        let mbr = generate(None, 512, 500);
        match inspect(&mbr, LAST) {
            MbrStatus::WrongSize { found, expected } => {
                assert_eq!(found, 500);
                assert_eq!(expected, LAST as u32);
            }
            other => panic!("expected WrongSize, got {:?}", other),
        }
    }

    #[test]
    fn hybrid_mbr_is_detected_and_never_repaired() {
        let mut mbr = generate(None, 512, LAST);
        // A second, real record next to the protective one.
        MbrRecord {
            boot_indicator: 0x80,
            start_chs: START_CHS,
            os_type: 0x07, // NTFS
            end_chs: END_CHS,
            starting_lba: 2048,
            size_in_lba: 4096,
        }
        .write_to(&mut mbr[OFF_RECORDS + RECORD_SIZE..OFF_RECORDS + 2 * RECORD_SIZE]);
        assert_eq!(inspect(&mbr, LAST), MbrStatus::Hybrid);
        assert!(!inspect(&mbr, LAST).needs_repair());
    }

    #[test]
    fn huge_disk_saturates_size_field() {
        assert_eq!(expected_size_in_lba(0xFFFF_FFFF_FFFF), u32::MAX);
    }
}
