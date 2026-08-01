//! Partition entries.

use crate::guid::Guid;
use alloc::string::String;
use alloc::vec::Vec;

/// The conventional entry size. Headers may declare larger; we handle that
/// by striding, never by assuming.
pub const ENTRY_SIZE_DEFAULT: u32 = 128;

/// Number of UTF-16 code units in the name field.
pub const NAME_LEN: usize = 36;

const OFF_TYPE_GUID: usize = 0;
const OFF_UNIQUE_GUID: usize = 16;
const OFF_STARTING_LBA: usize = 32;
const OFF_ENDING_LBA: usize = 40;
const OFF_ATTRIBUTES: usize = 48;
const OFF_NAME: usize = 56;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PartitionEntry {
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub attributes: u64,
    pub name: [u16; NAME_LEN],
}

impl PartitionEntry {
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 128 {
            return None;
        }
        let mut name = [0u16; NAME_LEN];
        for (i, slot) in name.iter_mut().enumerate() {
            let off = OFF_NAME + i * 2;
            *slot = u16::from_le_bytes([buf[off], buf[off + 1]]);
        }
        Some(PartitionEntry {
            type_guid: Guid::read_from(buf, OFF_TYPE_GUID)?,
            unique_guid: Guid::read_from(buf, OFF_UNIQUE_GUID)?,
            starting_lba: rd_u64(buf, OFF_STARTING_LBA),
            ending_lba: rd_u64(buf, OFF_ENDING_LBA),
            attributes: rd_u64(buf, OFF_ATTRIBUTES),
            name,
        })
    }

    /// An all-zero type GUID marks an unused slot.
    pub fn is_used(&self) -> bool {
        !self.type_guid.is_zero()
    }

    /// Decode the name, stopping at the first NUL. Unpaired surrogates are
    /// replaced rather than rejected, since this is only ever displayed.
    pub fn name_string(&self) -> String {
        let end = self.name.iter().position(|&c| c == 0).unwrap_or(NAME_LEN);
        char::decode_utf16(self.name[..end].iter().copied())
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    }

    /// Inclusive block count, or `None` if the range is inverted.
    pub fn block_count(&self) -> Option<u64> {
        self.ending_lba.checked_sub(self.starting_lba)?.checked_add(1)
    }

    /// True if this entry's block range intersects `other`'s.
    pub fn overlaps(&self, other: &PartitionEntry) -> bool {
        self.starting_lba <= other.ending_lba && other.starting_lba <= self.ending_lba
    }
}

/// Slice an entry array into entries, honouring a stride larger than 128.
///
/// Trailing bytes that do not form a whole entry are ignored rather than
/// treated as an error: the array is sized by the header, and a short read
/// is already reported elsewhere.
pub fn parse_array(bytes: &[u8], count: u32, stride: u32) -> Vec<PartitionEntry> {
    let stride = stride as usize;
    if stride < 128 {
        return Vec::new();
    }
    (0..count as usize)
        .filter_map(|i| {
            let start = i.checked_mul(stride)?;
            let end = start.checked_add(stride)?;
            PartitionEntry::parse(bytes.get(start..end)?)
        })
        .collect()
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    fn sample() -> Vec<u8> {
        let mut e = alloc::vec![0u8; 128];
        // ESP type GUID.
        e[0..16].copy_from_slice(
            Guid::from_fields(
                0xC12A_7328,
                0xF81F,
                0x11D2,
                [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
            )
            .as_bytes(),
        );
        e[32..40].copy_from_slice(&2048u64.to_le_bytes());
        e[40..48].copy_from_slice(&(2048u64 + 524_288 - 1).to_le_bytes());
        for (i, c) in "esp".encode_utf16().enumerate() {
            e[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        e
    }

    #[test]
    fn parses_name_and_extent() {
        let e = PartitionEntry::parse(&sample()).unwrap();
        assert!(e.is_used());
        assert_eq!(e.name_string(), "esp");
        assert_eq!(e.starting_lba, 2048);
        assert_eq!(e.block_count(), Some(524_288));
    }

    #[test]
    fn zero_entry_is_unused() {
        let e = PartitionEntry::parse(&[0u8; 128]).unwrap();
        assert!(!e.is_used());
        assert_eq!(e.name_string(), "");
    }

    #[test]
    fn inverted_range_has_no_block_count() {
        let mut raw = sample();
        raw[40..48].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(PartitionEntry::parse(&raw).unwrap().block_count(), None);
    }

    #[test]
    fn respects_a_stride_larger_than_the_entry() {
        let mut bytes = alloc::vec![0u8; 256];
        bytes[..128].copy_from_slice(&sample());
        // With stride 256 there is exactly one entry, not two.
        let entries = parse_array(&bytes, 1, 256);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name_string(), "esp");
    }

    #[test]
    fn overlap_detection_is_inclusive() {
        let mut a = PartitionEntry::parse(&sample()).unwrap();
        let mut b = a;
        a.starting_lba = 100;
        a.ending_lba = 199;
        b.starting_lba = 199;
        b.ending_lba = 299;
        assert!(a.overlaps(&b));
        b.starting_lba = 200;
        assert!(!a.overlaps(&b));
    }
}
