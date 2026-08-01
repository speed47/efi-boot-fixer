//! End-to-end repairs against real images, with sgdisk as the judge.

mod common;

use common::{foreign_image, steamos_image, Image, Op, BLOCK_SIZE};
use gptcore::crc::{Crc32, SoftCrc32};
use gptcore::header::GptHeader;
use gptcore::repair::{analyze, apply, plan, Implausible, Step, Verdict};
use gptcore::BlockDevice;

const CRC: SoftCrc32 = SoftCrc32;

/// Rewrite the backup table through a mutator, keeping both CRCs correct
/// so the backup still validates and reaches the plausibility checks.
fn patch_backup_entries(img: &Image, mutate: impl FnOnce(&mut [u8])) {
    let last = img.last_block();
    let raw = img.read_lba(last, 1);
    let header = GptHeader::parse(&raw).unwrap();
    let blocks = header.entry_array_blocks(BLOCK_SIZE).unwrap();
    let len = header.entry_array_len().unwrap();

    let mut entries = img.read_lba(header.partition_entry_lba, blocks);
    mutate(&mut entries);

    let mut fixed = header;
    fixed.partition_entry_array_crc32 = CRC.crc32(&entries[..len]);
    img.write_lba(header.partition_entry_lba, &entries);
    img.write_lba(last, &fixed.to_block(BLOCK_SIZE, &CRC));
}

fn set_entry_u64(entries: &mut [u8], index: usize, field_off: usize, value: u64) {
    let off = index * 128 + field_off;
    entries[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

const STARTING_LBA: usize = 32;
const ENDING_LBA: usize = 40;

/// Corrupt the primary, repair it, and assert sgdisk is satisfied and the
/// table came back identical.
fn assert_repairs(img: &Image, corrupt: impl FnOnce(&Image)) {
    let before = img.print();
    assert!(img.is_clean(), "fixture should start clean");

    corrupt(img);
    assert!(!img.is_clean(), "corruption did not actually break the primary");

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::PrimaryRepairable, "{:?}", analysis.verdict);

    let p = plan(&analysis, &CRC).expect("a repairable verdict must yield a plan");
    // The fields we promised to rewrite rather than copy.
    assert_eq!(p.header.my_lba, 1);
    assert_eq!(p.header.alternate_lba, dev.last_block());
    assert_eq!(p.header.partition_entry_lba, 2);

    apply(&mut dev, &p).unwrap();
    drop(dev);

    assert!(img.is_clean(), "sgdisk still unhappy after repair:\n{}", img.verify());
    assert_eq!(before, img.print(), "table differs after repair");
}

#[test]
fn healthy_image_needs_no_repair_and_is_not_written_to() {
    let img = steamos_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::Healthy);
    assert!(plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty(), "analysis must never write");
}

#[test]
fn zeroed_primary_header_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| i.zero_lba(1, 1));
}

#[test]
fn zeroed_primary_header_and_entry_array_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| i.zero_lba(1, 33));
}

#[test]
fn corrupted_primary_entry_array_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| i.write_lba(2, &[0x5A; 512]));
}

#[test]
fn bad_signature_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| {
        let mut b = i.read_lba(1, 1);
        b[..8].copy_from_slice(b"NOTAPART");
        i.write_lba(1, &b);
    });
}

#[test]
fn flipped_header_crc_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| {
        let mut b = i.read_lba(1, 1);
        b[16] ^= 0xFF;
        i.write_lba(1, &b);
    });
}

#[test]
fn primary_header_claiming_the_wrong_lba_is_repaired() {
    let img = steamos_image();
    assert_repairs(&img, |i| {
        // MyLBA says 5. Every CRC still checks out, so only the
        // read-from-LBA comparison catches this.
        let mut header = GptHeader::parse(&i.read_lba(1, 1)).unwrap();
        header.my_lba = 5;
        i.write_lba(1, &header.to_block(BLOCK_SIZE, &CRC));
    });
}

#[test]
fn repair_is_idempotent() {
    let img = steamos_image();
    assert_repairs(&img, |i| i.zero_lba(1, 1));

    let mut dev = img.disk();
    let second = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(second.verdict, Verdict::Healthy);
    assert!(plan(&second, &CRC).is_none());
}

#[test]
fn entry_array_is_written_and_flushed_before_the_header() {
    let img = steamos_image();
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let p = plan(&analysis, &CRC).unwrap();

    // Assert on the plan itself: the barrier is part of the data
    // structure, not an implementation detail of the executor.
    let kinds: Vec<String> = p
        .steps
        .iter()
        .map(|s| match s {
            Step::Write { lba, .. } => format!("write@{lba}"),
            Step::Flush { .. } => "flush".to_string(),
        })
        .collect();
    assert_eq!(kinds, vec!["write@2", "flush", "write@1", "flush"], "{kinds:?}");

    // And assert the executor honours it.
    dev.journal.clear();
    apply(&mut dev, &p).unwrap();
    let ops = dev.writes();
    let header_at = ops.iter().position(|o| matches!(o, Op::Write { lba: 1, .. })).unwrap();
    let entries_at = ops.iter().position(|o| matches!(o, Op::Write { lba: 2, .. })).unwrap();
    let barrier_at = ops.iter().position(|o| matches!(o, Op::Flush)).unwrap();
    assert!(entries_at < barrier_at && barrier_at < header_at, "{ops:?}");
}

#[test]
fn both_tables_destroyed_is_unrecoverable() {
    let img = steamos_image();
    img.zero_lba(1, 33);
    img.zero_lba(img.last_block(), 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::Unrecoverable);
    assert!(plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty());
}

#[test]
fn damaged_backup_alone_is_reported_but_not_repaired() {
    let img = steamos_image();
    img.zero_lba(img.last_block(), 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::BackupDegraded);
    assert!(plan(&analysis, &CRC).is_none());
}

#[test]
fn a_disk_that_is_not_steamos_is_refused() {
    let img = foreign_image();
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::RefusedImplausibleBackup);
    assert!(matches!(analysis.rejection, Some(Implausible::Unrecognized(_))));
    assert!(plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty());
}

#[test]
fn backup_with_overlapping_partitions_is_refused() {
    let img = steamos_image();
    // Drag rootfs-A back over efi-B. CRCs are fixed up, so the backup
    // header itself still validates: only the structural check sees this.
    patch_backup_entries(&img, |e| set_entry_u64(e, 3, STARTING_LBA, 657_408));
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::RefusedImplausibleBackup);
    assert!(
        matches!(analysis.rejection, Some(Implausible::Structure(_))),
        "{:?}",
        analysis.rejection
    );
    assert!(dev.writes().is_empty());
}

#[test]
fn backup_describing_a_partition_past_the_end_of_the_disk_is_refused() {
    let img = steamos_image();
    let last = img.last_block();
    patch_backup_entries(&img, |e| set_entry_u64(e, 9, ENDING_LBA, last + 4096));
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::RefusedImplausibleBackup);
    assert!(
        matches!(analysis.rejection, Some(Implausible::Structure(_))),
        "{:?}",
        analysis.rejection
    );
    assert!(dev.writes().is_empty());
}

#[test]
fn hybrid_mbr_is_refused_even_though_the_primary_is_broken() {
    let img = steamos_image();
    let mut mbr = img.read_lba(0, 1);
    // A real NTFS record beside the protective one.
    let rec = 446 + 16;
    mbr[rec] = 0x80;
    mbr[rec + 4] = 0x07;
    mbr[rec + 8..rec + 12].copy_from_slice(&2048u32.to_le_bytes());
    mbr[rec + 12..rec + 16].copy_from_slice(&524_288u32.to_le_bytes());
    img.write_lba(0, &mbr);
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::RefusedHybridMbr);
    assert!(plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty());
}

#[test]
fn a_wrong_protective_mbr_is_repaired_on_its_own() {
    let img = steamos_image();
    let mut mbr = img.read_lba(0, 1);
    mbr[446 + 12..446 + 16].copy_from_slice(&12_345u32.to_le_bytes());
    img.write_lba(0, &mbr);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::MbrOnly);

    let p = plan(&analysis, &CRC).unwrap();
    assert_eq!(p.writes().map(|(lba, _)| lba).collect::<Vec<_>>(), vec![0]);
    apply(&mut dev, &p).unwrap();
    drop(dev);

    assert!(img.is_clean(), "{}", img.verify());
    let mut dev = img.disk();
    assert_eq!(analyze(&mut dev, &CRC).unwrap().verdict, Verdict::Healthy);
}

#[test]
fn recovered_table_reports_the_expected_steamos_partitions() {
    let img = steamos_image();
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let rec = analysis.recognition.as_ref().expect("a repairable disk is assessed");
    assert!(rec.missing.is_empty(), "missing {:?}", rec.missing);
    assert!(rec.missing_critical.is_empty());

    let names: Vec<String> =
        analysis.source().unwrap().used_entries().map(|(_, e)| e.name_string()).collect();
    assert!(names.contains(&"esp".to_string()), "{names:?}");
    assert!(names.contains(&"rootfs-A".to_string()), "{names:?}");
    assert!(names.contains(&"home".to_string()), "{names:?}");
}
