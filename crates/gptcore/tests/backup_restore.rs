//! Saving both GPTs to a file and putting them back, against the real
//! Steam Deck fixture.
//!
//! sgdisk is the judge throughout: a restore that our own reader is happy
//! with proves nothing if an independent implementation disagrees.

mod common;

use common::{deck_corrupt_image, deck_image, Op};
use gptcore::backup::{
    self, decode, encode, restore_plan, DecodeError, Health, Mismatch, Role, Timestamp,
};
use gptcore::{analyze, apply, SoftCrc32};

const CRC: SoftCrc32 = SoftCrc32;

fn now() -> Timestamp {
    Timestamp { year: 2026, month: 8, day: 1, hour: 12, minute: 13, second: 14 }
}

fn meta() -> Vec<(String, String)> {
    vec![
        ("tool".to_string(), "efigptfix test".to_string()),
        ("firmware".to_string(), "Valve rev 0x10033".to_string()),
    ]
}

/// Capture, serialise and read back, exactly as the application does.
fn snapshot(img: &common::Image) -> backup::Archive {
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let archive = backup::capture(&mut disk, &analysis, now(), meta()).expect("capture");
    let bytes = encode(&archive, &CRC);
    decode(&bytes, &CRC).expect("decode what we just encoded")
}

#[test]
fn a_healthy_deck_is_captured_whole() {
    let img = deck_image();
    let archive = snapshot(&img);

    assert_eq!(archive.block_size, 512);
    assert_eq!(archive.last_block, img.last_block());
    assert_eq!(archive.health, Health::Healthy);

    // Every structure a GPT consists of, and nothing else.
    let mut roles: Vec<&'static str> = archive.chunks.iter().map(|c| c.role.describe()).collect();
    roles.sort_unstable();
    assert_eq!(
        roles,
        vec![
            "backup GPT header",
            "backup partition entry array",
            "primary GPT header",
            "primary partition entry array",
            "protective MBR",
        ]
    );

    let primary = archive.chunk(Role::PrimaryEntries).expect("primary array");
    assert_eq!(primary.lba, 2);
    assert_eq!(primary.data.len(), 32 * 512);
    assert_eq!(archive.chunk(Role::BackupHeader).unwrap().lba, img.last_block());

    // The bytes are the disk's, not a reconstruction.
    assert_eq!(primary.data, img.read_lba(2, 32));
    assert_eq!(archive.chunk(Role::PrimaryHeader).unwrap().data, img.read_lba(1, 1));
}

#[test]
fn restoring_onto_an_untouched_disk_changes_nothing() {
    let img = deck_image();
    let before = img.read_lba(0, 34);
    let archive = snapshot(&img);

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("plan");
    apply(&mut disk, &plan).expect("apply");

    assert_eq!(img.read_lba(0, 34), before);
    assert!(img.is_clean(), "{}", img.verify());
}

#[test]
fn a_wrecked_primary_is_put_back_exactly() {
    let img = deck_image();
    let before_head = img.read_lba(0, 34);
    let before_print = img.print();
    let archive = snapshot(&img);

    // Destroy the primary header and its entry array outright.
    img.zero_lba(1, 33);
    {
        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        assert!(analysis.primary.as_ref().is_ok_and(|t| !t.is_valid()));
    }

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("plan");
    apply(&mut disk, &plan).expect("apply");

    assert_eq!(img.read_lba(0, 34), before_head, "restore was not byte-exact");
    assert_eq!(img.print(), before_print);
    assert!(img.is_clean(), "{}", img.verify());
}

#[test]
fn entry_arrays_are_durable_before_the_headers_that_name_them() {
    let img = deck_image();
    let archive = snapshot(&img);
    img.zero_lba(1, 33);

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("plan");
    apply(&mut disk, &plan).expect("apply");

    let ops = disk.writes();
    let first_flush = ops.iter().position(|o| *o == Op::Flush).expect("a flush");
    let header_lbas = [1u64, img.last_block()];
    for (i, op) in ops.iter().enumerate() {
        if let Op::Write { lba, .. } = op {
            if header_lbas.contains(lba) {
                assert!(i > first_flush, "header at LBA {lba} was written before the flush");
            }
        }
    }
}

#[test]
fn a_snapshot_of_the_real_corruption_records_that_it_was_corrupt() {
    let img = deck_corrupt_image();
    let archive = snapshot(&img);

    assert_eq!(archive.health, Health::PrimaryCorrupt);
    assert!(!archive.health.is_clean());
    // The array really was at 2016 on this disk; the snapshot follows the
    // header rather than assuming LBA 2.
    assert_eq!(archive.chunk(Role::PrimaryEntries).unwrap().lba, 2016);

    let text = gptcore::style::plain(&backup::describe(&archive));
    assert!(text.contains("PRIMARY WAS CORRUPT"), "{text}");
    assert!(text.contains("Restoring it reinstates the damage"), "{text}");
}

#[test]
fn a_backup_from_a_differently_sized_disk_is_refused() {
    let img = deck_image();
    let archive = snapshot(&img);

    // Same image, but the device now claims to be smaller — the shape of a
    // drive swap, or of restoring the wrong file.
    let mut disk = common::FileDisk::open_truncated(&img.path, img.sectors - 4096).expect("open");
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    match restore_plan(&archive, &analysis) {
        Err(Mismatch::LastBlock { archive: a, disk: d }) => {
            assert_eq!(a, img.last_block());
            assert_eq!(d, img.sectors - 4096 - 1);
        }
        other => panic!("expected a geometry refusal, got {other:?}"),
    }
}

#[test]
fn a_damaged_backup_file_is_never_acted_on() {
    let img = deck_image();
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let archive = backup::capture(&mut disk, &analysis, now(), meta()).expect("capture");
    let mut bytes = encode(&archive, &CRC);

    // One bit inside a partition entry: the sort of damage that would
    // otherwise restore a plausible-looking but wrong table.
    let target = bytes.len() / 2;
    bytes[target] ^= 0x08;
    assert!(matches!(decode(&bytes, &CRC), Err(DecodeError::BadChecksum { .. })));
}

#[test]
fn a_restore_names_every_write_before_it_happens() {
    let img = deck_image();
    let archive = snapshot(&img);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("plan");

    let described: Vec<String> = plan.writes().map(|(lba, what)| format!("{lba} {what}")).collect();
    assert_eq!(described.len(), 5, "{described:?}");
    assert!(described.iter().any(|d| d.contains("protective MBR")));
    assert!(described.iter().any(|d| d.starts_with("1 primary GPT header")));
}

/// A genuine version-1 file: encode with no metadata, then strip the
/// (empty) metadata section and stamp the old version number.
fn downgrade_to_v1(archive: &backup::Archive) -> Vec<u8> {
    let mut bare = archive.clone();
    bare.meta.clear();
    let v2 = encode(&bare, &CRC);
    let body = &v2[..v2.len() - 4];
    let mut v1 = body[..body.len() - 4].to_vec();
    v1[8..12].copy_from_slice(&1u32.to_le_bytes());
    let sum = gptcore::Crc32::crc32(&CRC, &v1);
    v1.extend_from_slice(&sum.to_le_bytes());
    v1
}

#[test]
fn provenance_survives_the_round_trip() {
    let img = deck_image();
    let archive = snapshot(&img);
    assert_eq!(archive.version, backup::VERSION);
    assert_eq!(archive.meta_get("tool"), Some("efigptfix test"));
    assert_eq!(archive.meta_get("firmware"), Some("Valve rev 0x10033"));
    assert_eq!(archive.meta_get("nonexistent"), None);
}

#[test]
fn version_1_snapshots_are_still_readable() {
    let img = deck_image();
    let archive = snapshot(&img);
    let v1 = downgrade_to_v1(&archive);

    let old = decode(&v1, &CRC).expect("a v1 file must still decode");
    assert_eq!(old.version, 1);
    assert!(old.meta.is_empty());
    // Everything that matters for putting the disk back is still there.
    assert_eq!(old.last_block, archive.last_block);
    assert_eq!(old.disk_guid, archive.disk_guid);
    assert_eq!(old.chunks.len(), archive.chunks.len());

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    restore_plan(&old, &analysis).expect("a v1 snapshot must still be restorable");
}

#[test]
fn a_snapshot_recognises_the_disk_it_came_from() {
    let img = deck_image();
    let archive = snapshot(&img);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");

    let c = backup::compare(&archive, &analysis);
    assert_eq!(c.verdict(), backup::Match::SameDisk);
    assert!(c.geometry);
    assert!(c.disk_guid);
    assert_eq!(c.shared_partitions, c.archive_partitions);
    assert!(c.archive_partitions >= 8, "{c:?}");
}

#[test]
fn partition_guids_identify_the_disk_even_after_the_disk_guid_changes() {
    let img = deck_image();
    let mut archive = snapshot(&img);
    // What a partitioner rewriting only the disk GUID would leave behind.
    archive.disk_guid = gptcore::Guid::from_fields(0xDEAD, 0xBEEF, 1, [9; 8]);

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let c = backup::compare(&archive, &analysis);
    assert!(!c.disk_guid);
    assert_eq!(c.verdict(), backup::Match::SameDisk, "{c:?}");
}

#[test]
fn a_snapshot_from_a_different_disk_is_not_claimed() {
    let img = deck_image();
    let archive = snapshot(&img);

    let other = common::steamos_image();
    let mut disk = other.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let c = backup::compare(&archive, &analysis);
    assert_eq!(c.verdict(), backup::Match::DifferentDisk);
    assert_eq!(c.shared_partitions, 0);
}

#[test]
fn inspecting_a_snapshot_shows_what_identifies_it() {
    let img = deck_image();
    let archive = snapshot(&img);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let c = backup::compare(&archive, &analysis);

    let text = gptcore::style::plain(&backup::inspect(&archive, Some(("Disk 1", &c))));
    assert!(text.contains("Belongs to:"), "{text}");
    assert!(text.contains("efigptfix test"), "{text}");
    assert!(text.contains("Unique GUID"), "{text}");
    assert!(text.contains("rootfs-A"), "{text}");
    // The per-partition GUID is the evidence; it must actually be printed.
    let guid =
        archive.entries().iter().find(|e| e.name_string() == "rootfs-A").unwrap().unique_guid;
    assert!(text.contains(&guid.to_string()), "{text}");
}

#[test]
fn snapshot_names_count_up_and_never_reuse_a_number() {
    assert_eq!(backup::next_name(&[]).as_deref(), Some("gpt.001"));
    let taken = vec!["gpt.001".to_string(), "GPT.002".to_string(), "notes.txt".to_string()];
    assert_eq!(backup::next_name(&taken).as_deref(), Some("gpt.003"));

    // Deleting gpt.002 must not make the next one gpt.002 again: the
    // numbering is what tells you which snapshot is newest.
    let gapped = vec!["gpt.001".to_string(), "gpt.007".to_string()];
    assert_eq!(backup::next_name(&gapped).as_deref(), Some("gpt.008"));

    assert_eq!(backup::sequence_of("gpt.042"), Some(42));
    assert_eq!(backup::sequence_of("GPT.999"), Some(999));
    assert_eq!(backup::sequence_of("gpt.1"), None);
    assert_eq!(backup::sequence_of("gpt.abc"), None);
    assert_eq!(backup::sequence_of("snapshot.001"), None);

    let full = vec!["gpt.999".to_string()];
    assert_eq!(backup::next_name(&full), None);
}
