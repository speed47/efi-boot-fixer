//! The GPT header: parse, validate, re-serialize.
//!
//! Nothing here trusts the bytes it is handed. Every field that is later
//! used as a length or an offset is range-checked first, because the whole
//! point of this program is to run against a header that is known to be
//! damaged.

use crate::crc::Crc32;
use crate::guid::Guid;
use alloc::vec::Vec;
use core::fmt;

/// "EFI PART" read as a little-endian u64.
pub(crate) const GPT_SIGNATURE: u64 = 0x5452_4150_2049_4645;
pub(crate) const GPT_REVISION_1_0: u32 = 0x0001_0000;

/// Spec minimum. Headers may declare more, up to one block.
pub(crate) const HEADER_MIN_SIZE: u32 = 92;

/// Offsets of the fields we rewrite or zero, per UEFI spec table 5.5.
const OFF_SIGNATURE: usize = 0;
const OFF_REVISION: usize = 8;
const OFF_HEADER_SIZE: usize = 12;
const OFF_HEADER_CRC32: usize = 16;
const OFF_RESERVED: usize = 20;
const OFF_MY_LBA: usize = 24;
const OFF_ALTERNATE_LBA: usize = 32;
const OFF_FIRST_USABLE_LBA: usize = 40;
const OFF_LAST_USABLE_LBA: usize = 48;
const OFF_DISK_GUID: usize = 56;
const OFF_PARTITION_ENTRY_LBA: usize = 72;
const OFF_NUM_ENTRIES: usize = 80;
const OFF_ENTRY_SIZE: usize = 84;
const OFF_ENTRY_ARRAY_CRC32: usize = 88;

/// An upper bound on the entry array, so a corrupt count cannot make us
/// try to read gigabytes. 128 entries is conventional; the spec requires
/// firmware to support at least 16 KiB of entry array.
pub(crate) const MAX_ENTRY_COUNT: u32 = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GptHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub header_crc32: u32,
    pub reserved: u32,
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: Guid,
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub partition_entry_array_crc32: u32,
}

/// Everything that can be wrong with a header. Collected rather than
/// short-circuited so the report can show the operator the full picture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Defect {
    BadSignature {
        found: u64,
    },
    BadRevision {
        found: u32,
    },
    HeaderSizeOutOfRange {
        found: u32,
        block_size: u32,
    },
    ReservedNonZero {
        found: u32,
    },
    HeaderCrcMismatch {
        stored: u32,
        computed: u32,
    },
    MyLbaMismatch {
        stored: u64,
        read_from: u64,
    },
    AlternateLbaOutOfRange {
        stored: u64,
        last_block: u64,
    },
    EntrySizeInvalid {
        found: u32,
    },
    EntryCountOutOfRange {
        found: u32,
    },
    EntryArrayCrcMismatch {
        stored: u32,
        computed: u32,
    },
    UsableRangeInvalid {
        first: u64,
        last: u64,
        last_block: u64,
    },
    EntryArrayOutOfRange {
        entry_lba: u64,
        blocks: u64,
        last_block: u64,
    },
    /// The main entry array must start at LBA 2. Observed in the wild
    /// at 2016 (FirstUsableLBA - 32) with a correctly recomputed header
    /// CRC, so nothing but this check and the array CRC catches it.
    MainEntryLbaNotTwo {
        found: u64,
    },
}

impl fmt::Display for Defect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Defect::BadSignature { found } => {
                write!(f, "signature is {:#018x}, expected \"EFI PART\"", found)
            }
            Defect::BadRevision { found } => {
                write!(f, "revision is {:#010x}, expected 0x00010000", found)
            }
            Defect::HeaderSizeOutOfRange { found, block_size } => {
                write!(f, "HeaderSize is {}, must be {}..={}", found, HEADER_MIN_SIZE, block_size)
            }
            Defect::ReservedNonZero { found } => {
                write!(f, "reserved field is {:#010x}, must be zero", found)
            }
            Defect::HeaderCrcMismatch { stored, computed } => {
                write!(f, "header CRC32 is {:#010x}, recomputes to {:#010x}", stored, computed)
            }
            Defect::MyLbaMismatch { stored, read_from } => {
                write!(f, "MyLBA says {} but this header was read from LBA {}", stored, read_from)
            }
            Defect::AlternateLbaOutOfRange { stored, last_block } => {
                write!(f, "AlternateLBA {} is outside the disk (last block {})", stored, last_block)
            }
            Defect::EntrySizeInvalid { found } => write!(
                f,
                "SizeOfPartitionEntry is {}, must be a multiple of 8 and at least 128",
                found
            ),
            Defect::EntryCountOutOfRange { found } => {
                write!(f, "NumberOfPartitionEntries is {}, must be 1..={}", found, MAX_ENTRY_COUNT)
            }
            Defect::EntryArrayCrcMismatch { stored, computed } => {
                write!(f, "entry array CRC32 is {:#010x}, recomputes to {:#010x}", stored, computed)
            }
            Defect::UsableRangeInvalid { first, last, last_block } => write!(
                f,
                "usable range {}..={} is not sane for a disk whose last block is {}",
                first, last, last_block
            ),
            Defect::EntryArrayOutOfRange { entry_lba, blocks, last_block } => write!(
                f,
                "entry array at LBA {} spanning {} blocks does not fit before block {}",
                entry_lba, blocks, last_block
            ),
            Defect::MainEntryLbaNotTwo { found } => {
                write!(f, "main PartitionEntryLBA is {}, must be 2", found)
            }
        }
    }
}

impl GptHeader {
    /// Decode the fixed 92-byte prefix. `buf` must be at least that long.
    ///
    /// This never fails on content: a header full of garbage still parses,
    /// because [`validate`](GptHeader::validate) is what decides whether the
    /// content is usable. It only fails if the buffer is too short.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_MIN_SIZE as usize {
            return None;
        }
        Some(GptHeader {
            signature: rd_u64(buf, OFF_SIGNATURE),
            revision: rd_u32(buf, OFF_REVISION),
            header_size: rd_u32(buf, OFF_HEADER_SIZE),
            header_crc32: rd_u32(buf, OFF_HEADER_CRC32),
            reserved: rd_u32(buf, OFF_RESERVED),
            my_lba: rd_u64(buf, OFF_MY_LBA),
            alternate_lba: rd_u64(buf, OFF_ALTERNATE_LBA),
            first_usable_lba: rd_u64(buf, OFF_FIRST_USABLE_LBA),
            last_usable_lba: rd_u64(buf, OFF_LAST_USABLE_LBA),
            disk_guid: Guid::read_from(buf, OFF_DISK_GUID).expect("length checked above"),
            partition_entry_lba: rd_u64(buf, OFF_PARTITION_ENTRY_LBA),
            number_of_partition_entries: rd_u32(buf, OFF_NUM_ENTRIES),
            size_of_partition_entry: rd_u32(buf, OFF_ENTRY_SIZE),
            partition_entry_array_crc32: rd_u32(buf, OFF_ENTRY_ARRAY_CRC32),
        })
    }

    /// Byte length of the entry array, or `None` if the declared geometry
    /// overflows or exceeds [`MAX_ENTRY_COUNT`].
    pub fn entry_array_len(&self) -> Option<usize> {
        if self.number_of_partition_entries == 0
            || self.number_of_partition_entries > MAX_ENTRY_COUNT
        {
            return None;
        }
        if self.size_of_partition_entry < 128 || !self.size_of_partition_entry.is_multiple_of(8) {
            return None;
        }
        (self.number_of_partition_entries as usize)
            .checked_mul(self.size_of_partition_entry as usize)
    }

    /// How many blocks the entry array occupies, rounded up.
    ///
    /// `None` for a zero block size: `div_ceil` would panic, and a panic in
    /// firmware is an unrecoverable hang.
    pub fn entry_array_blocks(&self, block_size: u32) -> Option<u64> {
        if block_size == 0 {
            return None;
        }
        let len = self.entry_array_len()? as u64;
        Some(len.div_ceil(block_size as u64))
    }

    /// Structural checks that do not need the header bytes or the entry
    /// array. `read_from` is the LBA this header was actually read from,
    /// which is what makes a swapped or copied header detectable.
    fn validate_fields(
        &self,
        read_from: u64,
        last_block: u64,
        block_size: u32,
        out: &mut Vec<Defect>,
    ) {
        if self.signature != GPT_SIGNATURE {
            out.push(Defect::BadSignature { found: self.signature });
        }
        if self.revision != GPT_REVISION_1_0 {
            out.push(Defect::BadRevision { found: self.revision });
        }
        if self.header_size < HEADER_MIN_SIZE || self.header_size > block_size {
            out.push(Defect::HeaderSizeOutOfRange { found: self.header_size, block_size });
        }
        if self.reserved != 0 {
            out.push(Defect::ReservedNonZero { found: self.reserved });
        }
        if self.my_lba != read_from {
            out.push(Defect::MyLbaMismatch { stored: self.my_lba, read_from });
        }
        if self.alternate_lba == 0 || self.alternate_lba > last_block {
            out.push(Defect::AlternateLbaOutOfRange { stored: self.alternate_lba, last_block });
        }
        if self.size_of_partition_entry < 128 || !self.size_of_partition_entry.is_multiple_of(8) {
            out.push(Defect::EntrySizeInvalid { found: self.size_of_partition_entry });
        }
        if self.number_of_partition_entries == 0
            || self.number_of_partition_entries > MAX_ENTRY_COUNT
        {
            out.push(Defect::EntryCountOutOfRange { found: self.number_of_partition_entries });
        }
        if self.first_usable_lba < 2
            || self.last_usable_lba > last_block
            || self.first_usable_lba > self.last_usable_lba
        {
            out.push(Defect::UsableRangeInvalid {
                first: self.first_usable_lba,
                last: self.last_usable_lba,
                last_block,
            });
        }
        // Only the main GPT is pinned to LBA 2; the secondary's array
        // sits just below its header at the end of the disk.
        if read_from == 1 && self.partition_entry_lba != 2 {
            out.push(Defect::MainEntryLbaNotTwo { found: self.partition_entry_lba });
        }
        if let Some(blocks) = self.entry_array_blocks(block_size) {
            let end = self.partition_entry_lba.checked_add(blocks);
            let fits = self.partition_entry_lba >= 1 && end.is_some_and(|e| e <= last_block + 1);
            if !fits {
                out.push(Defect::EntryArrayOutOfRange {
                    entry_lba: self.partition_entry_lba,
                    blocks,
                    last_block,
                });
            }
        }
    }

    /// Recompute the header CRC32 over `header_size` bytes with the CRC
    /// field zeroed, as the spec requires.
    ///
    /// Returns `None` if `header_size` is not a usable length for `raw`,
    /// which is why the caller must treat an out-of-range HeaderSize as
    /// fatal before relying on the CRC.
    pub(crate) fn compute_header_crc(&self, raw: &[u8], crc: &impl Crc32) -> Option<u32> {
        let size = self.header_size as usize;
        if size < HEADER_MIN_SIZE as usize || size > raw.len() {
            return None;
        }
        let mut scratch = Vec::from(&raw[..size]);
        scratch[OFF_HEADER_CRC32..OFF_HEADER_CRC32 + 4].fill(0);
        Some(crc.crc32(&scratch))
    }

    /// Full validation. `raw` is the block the header came from, `entries`
    /// the entry array bytes if they could be read.
    pub fn validate(
        &self,
        raw: &[u8],
        entries: Option<&[u8]>,
        read_from: u64,
        last_block: u64,
        block_size: u32,
        crc: &impl Crc32,
    ) -> Vec<Defect> {
        let mut defects = Vec::new();
        self.validate_fields(read_from, last_block, block_size, &mut defects);

        match self.compute_header_crc(raw, crc) {
            Some(computed) if computed != self.header_crc32 => {
                defects.push(Defect::HeaderCrcMismatch { stored: self.header_crc32, computed });
            }
            // A HeaderSize we cannot use was already reported as a defect.
            _ => {}
        }

        if let (Some(entries), Some(len)) = (entries, self.entry_array_len()) {
            if entries.len() >= len {
                let computed = crc.crc32(&entries[..len]);
                if computed != self.partition_entry_array_crc32 {
                    defects.push(Defect::EntryArrayCrcMismatch {
                        stored: self.partition_entry_array_crc32,
                        computed,
                    });
                }
            }
        }

        defects
    }

    /// Serialize into a full block, zero-padded.
    ///
    /// `header_crc32` is recomputed here rather than taken from `self`, so
    /// a caller cannot write a header whose CRC does not match its body.
    pub fn to_block(&self, block_size: u32, crc: &impl Crc32) -> Vec<u8> {
        let mut block = alloc::vec![0u8; block_size as usize];
        // Not `clamp`: it panics when min > max, which a device reporting a
        // block size below the 92-byte minimum would trigger. In firmware a
        // panic is an unrecoverable hang, so saturate instead.
        let size =
            self.header_size.max(HEADER_MIN_SIZE).min(block_size.max(HEADER_MIN_SIZE)) as usize;
        if block.len() < size {
            block.resize(size, 0);
        }

        wr_u64(&mut block, OFF_SIGNATURE, self.signature);
        wr_u32(&mut block, OFF_REVISION, self.revision);
        wr_u32(&mut block, OFF_HEADER_SIZE, size as u32);
        wr_u32(&mut block, OFF_HEADER_CRC32, 0);
        wr_u32(&mut block, OFF_RESERVED, 0);
        wr_u64(&mut block, OFF_MY_LBA, self.my_lba);
        wr_u64(&mut block, OFF_ALTERNATE_LBA, self.alternate_lba);
        wr_u64(&mut block, OFF_FIRST_USABLE_LBA, self.first_usable_lba);
        wr_u64(&mut block, OFF_LAST_USABLE_LBA, self.last_usable_lba);
        block[OFF_DISK_GUID..OFF_DISK_GUID + 16].copy_from_slice(self.disk_guid.as_bytes());
        wr_u64(&mut block, OFF_PARTITION_ENTRY_LBA, self.partition_entry_lba);
        wr_u32(&mut block, OFF_NUM_ENTRIES, self.number_of_partition_entries);
        wr_u32(&mut block, OFF_ENTRY_SIZE, self.size_of_partition_entry);
        wr_u32(&mut block, OFF_ENTRY_ARRAY_CRC32, self.partition_entry_array_crc32);

        let computed = crc.crc32(&block[..size]);
        wr_u32(&mut block, OFF_HEADER_CRC32, computed);
        block
    }
}

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(bytes)
}

fn wr_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn wr_u64(buf: &mut [u8], off: usize, value: u64) {
    buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::SoftCrc32;
    extern crate std;

    fn sample_header() -> GptHeader {
        GptHeader {
            signature: GPT_SIGNATURE,
            revision: GPT_REVISION_1_0,
            header_size: HEADER_MIN_SIZE,
            header_crc32: 0,
            reserved: 0,
            my_lba: 1,
            alternate_lba: 1000,
            first_usable_lba: 34,
            last_usable_lba: 966,
            disk_guid: Guid::from_fields(1, 2, 3, [4; 8]),
            partition_entry_lba: 2,
            number_of_partition_entries: 128,
            size_of_partition_entry: 128,
            partition_entry_array_crc32: 0,
        }
    }

    /// A device reporting a block size below the 92-byte header minimum
    /// used to reach `u32::clamp(92, block_size)`, which panics when
    /// min > max. In firmware a panic is an unrecoverable hang, so this
    /// path has to saturate instead.
    #[test]
    fn to_block_survives_an_absurdly_small_block_size() {
        for block_size in [0u32, 1, 64, 91] {
            let block = sample_header().to_block(block_size, &SoftCrc32);
            assert!(
                block.len() >= HEADER_MIN_SIZE as usize,
                "block_size {block_size} produced {} bytes",
                block.len()
            );
        }
    }

    #[test]
    fn round_trips_through_a_block() {
        let original = sample_header();
        let block = original.to_block(512, &SoftCrc32);
        let parsed = GptHeader::parse(&block).unwrap();
        assert_eq!(parsed.my_lba, 1);
        assert_eq!(parsed.alternate_lba, 1000);
        assert_eq!(parsed.partition_entry_lba, 2);
        assert_eq!(parsed.disk_guid, original.disk_guid);
        // to_block recomputes the CRC, so the parsed header validates.
        let computed = parsed.compute_header_crc(&block, &SoftCrc32).unwrap();
        assert_eq!(computed, parsed.header_crc32);
    }

    #[test]
    fn a_short_buffer_is_rejected_rather_than_indexed() {
        assert!(GptHeader::parse(&[0u8; 91]).is_none());
        assert!(GptHeader::parse(&[]).is_none());
        assert!(GptHeader::parse(&[0u8; 92]).is_some());
    }

    #[test]
    fn absurd_entry_geometry_does_not_overflow() {
        let mut h = sample_header();
        h.number_of_partition_entries = u32::MAX;
        h.size_of_partition_entry = u32::MAX;
        assert_eq!(h.entry_array_len(), None);
        assert_eq!(h.entry_array_blocks(512), None);
    }

    /// `div_ceil` panics on a zero divisor, so a device reporting a zero
    /// block size must not reach it.
    #[test]
    fn zero_block_size_does_not_divide_by_zero() {
        assert_eq!(sample_header().entry_array_blocks(0), None);
    }
}
