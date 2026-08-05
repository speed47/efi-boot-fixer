//! The 4096-byte-sector paths, which no other fixture covers.
//!
//! Every image test in this suite runs at 512 bytes a block, while the
//! application accepts anything from 512 to 65536 — so a units slip on the
//! 4Kn path (bytes where blocks were meant, a hardcoded 512) would pass
//! the whole suite. sgdisk cannot be the oracle here: pointed at an image
//! *file* it assumes 512-byte sectors. So this table is built by hand from
//! the spec, and the assertions are about arithmetic — every LBA and every
//! length the tool derives has to move with the block size.

use gptcore::backup::{self, Role, Timestamp};
use gptcore::mbr::{self, MbrStatus};
use gptcore::{analyze, apply, plan, BlockDevice, GptHeader, Guid, IoError, SoftCrc32, Verdict};

const CRC: SoftCrc32 = SoftCrc32;
const BS: u32 = 4096;
/// 128 MiB: big enough for a plausible layout, small enough to hold in RAM.
const BLOCKS: u64 = 32 * 1024;
const LAST: u64 = BLOCKS - 1;
/// 128 entries x 128 bytes = 4 blocks at this size (32 at 512).
const ARRAY_BLOCKS: u64 = 4;

struct MemDisk {
    data: Vec<u8>,
}

impl BlockDevice for MemDisk {
    fn block_size(&self) -> u32 {
        BS
    }

    fn last_block(&self) -> u64 {
        LAST
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let start = usize::try_from(lba * BS as u64).map_err(|_| IoError::OutOfRange)?;
        let end = start.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
        buf.copy_from_slice(self.data.get(start..end).ok_or(IoError::OutOfRange)?);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), IoError> {
        let start = usize::try_from(lba * BS as u64).map_err(|_| IoError::OutOfRange)?;
        let end = start.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
        self.data.get_mut(start..end).ok_or(IoError::OutOfRange)?.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

fn entry(type_guid: Guid, unique: Guid, start: u64, end: u64, name: &str) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[0..16].copy_from_slice(type_guid.as_bytes());
    e[16..32].copy_from_slice(unique.as_bytes());
    e[32..40].copy_from_slice(&start.to_le_bytes());
    e[40..48].copy_from_slice(&end.to_le_bytes());
    for (i, c) in name.encode_utf16().enumerate() {
        e[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
    }
    e
}

/// A healthy 4Kn disk: protective MBR, both headers, both entry arrays,
/// holding the two partitions the recognizer treats as critical.
fn disk_4kn() -> MemDisk {
    let esp_type = Guid::from_fields(
        0xC12A_7328,
        0xF81F,
        0x11D2,
        [0xBA, 0x4B, 0, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
    );
    let root_type = Guid::from_fields(
        0x4F68_BCE3,
        0xE8CD,
        0x4DB1,
        [0x96, 0xE7, 0xFB, 0xCA, 0xF9, 0x84, 0xB7, 0x09],
    );

    let first_usable = 256u64;
    let last_usable = LAST - ARRAY_BLOCKS - 1;

    let mut array = vec![0u8; (ARRAY_BLOCKS * BS as u64) as usize];
    array[..128].copy_from_slice(&entry(
        esp_type,
        Guid::from_fields(1, 2, 3, [4; 8]),
        first_usable,
        first_usable + 1023,
        "esp",
    ));
    array[128..256].copy_from_slice(&entry(
        root_type,
        Guid::from_fields(5, 6, 7, [8; 8]),
        first_usable + 1024,
        last_usable,
        "rootfs-A",
    ));
    let array_crc = gptcore::Crc32::crc32(&CRC, &array[..128 * 128]);

    let header = |my_lba: u64, alternate: u64, entry_lba: u64| GptHeader {
        signature: 0x5452_4150_2049_4645,
        revision: 0x0001_0000,
        header_size: 92,
        header_crc32: 0, // recomputed by to_block
        reserved: 0,
        my_lba,
        alternate_lba: alternate,
        first_usable_lba: first_usable,
        last_usable_lba: last_usable,
        disk_guid: Guid::from_fields(9, 10, 11, [12; 8]),
        partition_entry_lba: entry_lba,
        number_of_partition_entries: 128,
        size_of_partition_entry: 128,
        partition_entry_array_crc32: array_crc,
    };

    let mut disk = MemDisk { data: vec![0u8; (BLOCKS * BS as u64) as usize] };
    disk.write_blocks(0, &mbr::generate(None, BS, LAST)).unwrap();
    disk.write_blocks(1, &header(1, LAST, 2).to_block(BS, &CRC)).unwrap();
    disk.write_blocks(2, &array).unwrap();
    disk.write_blocks(LAST - ARRAY_BLOCKS, &array).unwrap();
    disk.write_blocks(LAST, &header(LAST, 1, LAST - ARRAY_BLOCKS).to_block(BS, &CRC)).unwrap();
    disk
}

#[test]
fn a_4kn_disk_analyzes_healthy() {
    let mut disk = disk_4kn();
    assert_eq!(mbr::inspect(&disk.data[..BS as usize], LAST), MbrStatus::Protective);

    let analysis = analyze(&mut disk, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::Healthy, "{:?}", analysis.main);
    let main = analysis.main.as_ref().unwrap();
    // The array is 4 blocks here, not the 32 it is at 512 bytes a block.
    assert_eq!(main.header.entry_array_blocks(BS), Some(ARRAY_BLOCKS));
    assert_eq!(main.entries_raw.len(), 128 * 128);
}

#[test]
fn a_4kn_main_gpt_is_rebuilt_from_the_secondary() {
    let mut disk = disk_4kn();
    disk.write_blocks(1, &vec![0u8; BS as usize]).unwrap();

    let analysis = analyze(&mut disk, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::MainRepairable, "{:?}", analysis.rejection);

    let repair = plan(&analysis, &CRC).expect("a plan");
    assert_eq!(repair.header.alternate_lba, LAST);
    assert_eq!(repair.header.partition_entry_lba, 2);
    // The array write must be sized in this disk's blocks: 4 x 4096 bytes.
    let array_write =
        repair.writes().find(|(lba, _)| *lba == 2).expect("an entry array write at LBA 2");
    assert!(array_write.1.contains("4 blocks"), "{}", array_write.1);

    apply(&mut disk, &repair).unwrap();
    let after = analyze(&mut disk, &CRC).unwrap();
    assert_eq!(after.verdict, Verdict::Healthy);
}

#[test]
fn a_4kn_snapshot_round_trips_and_restores() {
    let mut disk = disk_4kn();
    let analysis = analyze(&mut disk, &CRC).unwrap();
    let time = Timestamp { year: 2026, month: 8, day: 5, hour: 1, minute: 2, second: 3 };
    let archive = backup::capture(&mut disk, &analysis, time, Vec::new()).unwrap();
    let archive = backup::decode(&backup::encode(&archive, &CRC), &CRC).unwrap();

    assert_eq!(archive.block_size, BS);
    let main = archive.chunk(Role::MainEntries).unwrap();
    assert_eq!(main.lba, 2);
    assert_eq!(main.blocks(BS), ARRAY_BLOCKS);
    assert_eq!(archive.chunk(Role::SecondaryEntries).unwrap().lba, LAST - ARRAY_BLOCKS);

    // Wreck the main GPT, then put the snapshot back.
    disk.write_blocks(1, &vec![0u8; (ARRAY_BLOCKS + 1) as usize * BS as usize]).unwrap();
    let analysis = analyze(&mut disk, &CRC).unwrap();
    let restore = backup::restore_plan(&archive, &analysis).expect("restorable");
    apply(&mut disk, &restore).unwrap();
    assert_eq!(analyze(&mut disk, &CRC).unwrap().verdict, Verdict::Healthy);
}

#[test]
fn a_4kn_gap_is_closed_with_this_disks_arithmetic() {
    let mut disk = disk_4kn();
    let analysis = analyze(&mut disk, &CRC).unwrap();
    // 2 + 4 array blocks, not the 34 a 512-byte disk would propose.
    assert_eq!(
        gptcore::prevent::assess(&analysis),
        gptcore::prevent::Verdict::Applicable { current: 256, proposed: 2 + ARRAY_BLOCKS }
    );

    let gap = gptcore::prevent::plan(&analysis, &CRC).expect("a plan");
    apply(&mut disk, &gap).unwrap();

    let after = analyze(&mut disk, &CRC).unwrap();
    assert_eq!(after.verdict, Verdict::Healthy);
    assert_eq!(after.main.as_ref().unwrap().header.first_usable_lba, 2 + ARRAY_BLOCKS);
    assert_eq!(
        gptcore::prevent::assess(&after),
        gptcore::prevent::Verdict::AlreadyMinimal {
            current: 2 + ARRAY_BLOCKS,
            proposed: 2 + ARRAY_BLOCKS
        }
    );
}
