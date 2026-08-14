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
        ("tool".to_string(), "bootfixr test".to_string()),
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
            "main GPT header",
            "main partition entry array",
            "protective MBR",
            "secondary GPT header",
            "secondary partition entry array",
        ]
    );

    let main = archive.chunk(Role::MainEntries).expect("main array");
    assert_eq!(main.lba, 2);
    assert_eq!(main.data.len(), 32 * 512);
    assert_eq!(archive.chunk(Role::SecondaryHeader).unwrap().lba, img.last_block());

    // The bytes are the disk's, not a reconstruction.
    assert_eq!(main.data, img.read_lba(2, 32));
    assert_eq!(archive.chunk(Role::MainHeader).unwrap().data, img.read_lba(1, 1));
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
fn a_wrecked_main_gpt_is_put_back_exactly() {
    let img = deck_image();
    let before_head = img.read_lba(0, 34);
    let before_print = img.print();
    let archive = snapshot(&img);

    // Destroy the main GPT header and its entry array outright.
    img.zero_lba(1, 33);
    {
        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        assert!(analysis.main.as_ref().is_ok_and(|t| !t.is_valid()));
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

    assert_eq!(archive.health, Health::MainCorrupt);
    assert!(!archive.health.is_clean());
    // The array really was at 2016 on this disk; the snapshot follows the
    // header rather than assuming LBA 2.
    assert_eq!(archive.chunk(Role::MainEntries).unwrap().lba, 2016);

    let text = gptcore::style::plain(&backup::describe(&archive));
    assert!(text.contains("MAIN GPT WAS CORRUPT"), "{text}");
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
    assert!(described.iter().any(|d| d.starts_with("1 main GPT header")));
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
    assert_eq!(archive.meta_get("tool"), Some("bootfixr test"));
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
    assert!(text.contains("bootfixr test"), "{text}");
    assert!(text.contains("Unique GUID"), "{text}");
    assert!(text.contains("rootfs-A"), "{text}");
    // The per-partition GUID is the evidence; it must actually be printed.
    let guid =
        archive.entries().iter().find(|e| e.name_string() == "rootfs-A").unwrap().unique_guid;
    assert!(text.contains(&guid.to_string()), "{text}");
}

#[test]
fn snapshot_names_count_up_and_never_reuse_a_number() {
    assert_eq!(backup::next_name(&[]).as_deref(), Some("gpt-001.bkp"));
    let taken = vec!["gpt-001.bkp".to_string(), "GPT-002.BKP".to_string(), "notes.txt".to_string()];
    assert_eq!(backup::next_name(&taken).as_deref(), Some("gpt-003.bkp"));

    // Deleting gpt-002.bkp must not make the next one gpt-002.bkp again: the
    // numbering is what tells you which snapshot is newest.
    let gapped = vec!["gpt-001.bkp".to_string(), "gpt-007.bkp".to_string()];
    assert_eq!(backup::next_name(&gapped).as_deref(), Some("gpt-008.bkp"));

    assert_eq!(backup::sequence_of("gpt-042.bkp"), Some(42));
    assert_eq!(backup::sequence_of("GPT-999.BKP"), Some(999));
    assert_eq!(backup::sequence_of("gpt-1.bkp"), None);
    assert_eq!(backup::sequence_of("gpt-abc.bkp"), None);
    assert_eq!(backup::sequence_of("snapshot-001.bkp"), None);

    let full = vec!["gpt-999.bkp".to_string()];
    assert_eq!(backup::next_name(&full), None);
}

/// The refusal list applies to every write path. A hybrid MBR set up after
/// the snapshot was taken must stop the restore, exactly as it stops a
/// repair and a prevention.
#[test]
fn a_hybrid_mbr_on_the_target_refuses_the_restore() {
    let img = deck_image();
    let archive = snapshot(&img);

    // A real NTFS record beside the protective one.
    let mut mbr = img.read_lba(0, 1);
    let rec = 446 + 16;
    mbr[rec] = 0x80;
    mbr[rec + 4] = 0x07;
    mbr[rec + 8..rec + 12].copy_from_slice(&2048u32.to_le_bytes());
    mbr[rec + 12..rec + 16].copy_from_slice(&524_288u32.to_le_bytes());
    img.write_lba(0, &mbr);

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    assert!(matches!(restore_plan(&archive, &analysis), Err(Mismatch::HybridMbr)));
}

/// And the same refusal from the other side. Capture records the MBR as it
/// found it, so a snapshot of a disk that carried a hybrid MBR holds one —
/// and writing it back leaves a disk that repair, prevent and restore all
/// refuse from then on, with nothing on screen having said so.
#[test]
fn a_hybrid_mbr_inside_the_snapshot_refuses_the_restore() {
    let img = deck_image();
    let mut archive = snapshot(&img);

    // A second, non-protective record alongside the 0xEE one, in the
    // snapshot rather than on the disk.
    let chunk = archive.chunks.iter_mut().find(|c| c.role == Role::Mbr).expect("mbr chunk");
    let rec = 446 + 16;
    chunk.data[rec] = 0x80;
    chunk.data[rec + 4] = 0x07;
    chunk.data[rec + 8..rec + 12].copy_from_slice(&2048u32.to_le_bytes());
    chunk.data[rec + 12..rec + 16].copy_from_slice(&524_288u32.to_le_bytes());

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    assert_eq!(
        analysis.mbr,
        gptcore::mbr::MbrStatus::Protective,
        "the disk itself is fine; the refusal has to come from the file"
    );
    assert!(matches!(restore_plan(&archive, &analysis), Err(Mismatch::HybridMbrInArchive)));
}

/// A corrupt main header is free to point its entry array anywhere,
/// including into the middle of a partition. Capturing from there means
/// restoring to there, so the pointer must not be followed.
#[test]
fn a_pointer_into_a_partition_is_not_followed_by_capture() {
    let img = deck_image();
    let mut header = gptcore::GptHeader::parse(&img.read_lba(1, 1)).expect("parse");
    // Well inside rootfs-A, with the CRC resealed so only the array
    // checks can notice — the shape of the real corruption, aimed worse.
    header.partition_entry_lba = 1_000_000;
    img.write_lba(1, &header.to_block(512, &CRC));

    let archive = snapshot(&img);
    assert_eq!(archive.health, Health::MainCorrupt);
    assert_eq!(
        archive.chunk(Role::MainEntries).expect("captured conventionally").lba,
        2,
        "a pointer into the partition area must not drive the capture"
    );
}

/// The same guard on the way back, for snapshots old builds already wrote:
/// an entry-array chunk recorded inside the partition area is refused.
#[test]
fn an_archived_array_inside_the_partition_area_is_refused() {
    let img = deck_image();
    let mut archive = snapshot(&img);
    let chunk =
        archive.chunks.iter_mut().find(|c| c.role == Role::MainEntries).expect("main array");
    chunk.lba = 1_000_000;

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    match restore_plan(&archive, &analysis) {
        Err(Mismatch::OverlapsPartitions { role: Role::MainEntries, lba: 1_000_000, .. }) => {}
        other => panic!("expected an overlap refusal, got {other:?}"),
    }
}

/// A snapshot's headers are weaker evidence than a live disk's, so while
/// the disk can still speak it is the disk that is asked. Declaring a
/// usable range of 2..2 passes every sanity test and then vetoes nothing,
/// which is a blessing for any LBA above 2 — and is refused here because
/// the disk in front of us says rootfs-A is there.
#[test]
fn an_archives_own_header_cannot_authorise_its_array_into_a_partition() {
    let img = deck_image();
    let mut archive = snapshot(&img);

    // Nothing the archive says can object to anything above LBA 2 ...
    for role in [Role::MainHeader, Role::SecondaryHeader] {
        let chunk = archive.chunks.iter_mut().find(|c| c.role == role).expect("header chunk");
        let mut header = gptcore::GptHeader::parse(&chunk.data).expect("parse");
        header.first_usable_lba = 2;
        header.last_usable_lba = 2;
        chunk.data = header.to_block(512, &CRC);
    }
    // ... and the array is aimed well inside rootfs-A.
    let chunk =
        archive.chunks.iter_mut().find(|c| c.role == Role::MainEntries).expect("main array");
    chunk.lba = 1_000_000;

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    match restore_plan(&archive, &analysis) {
        Err(Mismatch::OverlapsPartitions { role: Role::MainEntries, lba: 1_000_000, .. }) => {}
        other => panic!("expected an overlap refusal, got {other:?}"),
    }
}

/// A disk with nothing left to read is the case the tool exists for, and
/// it is also the case where no header on the disk can say where the
/// partitions are. A snapshot recorded at the conventional locations still
/// goes back.
#[test]
fn a_snapshot_restores_onto_a_disk_with_no_readable_table() {
    let img = deck_image();
    let before = img.read_lba(0, 34);
    let archive = snapshot(&img);

    img.zero_lba(1, 33);
    img.zero_lba(img.last_block() - 32, 33);
    {
        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        assert!(analysis.main.as_ref().is_ok_and(|t| !t.is_valid()));
        assert!(analysis.secondary.as_ref().is_ok_and(|t| !t.is_valid()));
    }

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("plan");
    apply(&mut disk, &plan).expect("apply");

    assert_eq!(img.read_lba(0, 34), before, "restore was not byte-exact");
    assert!(img.is_clean(), "{}", img.verify());
}

/// The same trick on a disk with nothing left to contradict it. The usable
/// range is no longer the only thing the snapshot is asked for: the entries
/// beside it still put rootfs-A at that LBA, and a snapshot vouching for
/// its own array has to clear those too.
#[test]
fn a_narrowed_usable_range_still_cannot_bless_an_array_into_a_partition() {
    let img = deck_image();
    let mut archive = snapshot(&img);
    for role in [Role::MainHeader, Role::SecondaryHeader] {
        let chunk = archive.chunks.iter_mut().find(|c| c.role == role).expect("header chunk");
        let mut header = gptcore::GptHeader::parse(&chunk.data).expect("parse");
        header.first_usable_lba = 2;
        header.last_usable_lba = 2;
        chunk.data = header.to_block(512, &CRC);
    }
    let chunk =
        archive.chunks.iter_mut().find(|c| c.role == Role::MainEntries).expect("main array");
    chunk.lba = 1_000_000;

    // Both tables gone, so nothing on the disk can object.
    img.zero_lba(1, 1);
    img.zero_lba(img.last_block(), 1);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    match restore_plan(&archive, &analysis) {
        Err(Mismatch::OverlapsPartitions { role: Role::MainEntries, lba: 1_000_000, .. }) => {}
        other => panic!("expected an overlap refusal, got {other:?}"),
    }
}

/// With no readable table on the disk and none in the file either, an
/// unconventional array is refused — but not for overlapping anything,
/// because nothing has said a partition is there. The two refusals read
/// differently on purpose: one says where the partitions are, the other
/// says nobody knows.
#[test]
fn an_array_nothing_can_vouch_for_is_refused_without_accusing_it_of_overlapping() {
    let img = deck_image();
    let mut archive = snapshot(&img);
    // Neither header offers a usable range worth consulting.
    for role in [Role::MainHeader, Role::SecondaryHeader] {
        let chunk = archive.chunks.iter_mut().find(|c| c.role == role).expect("header chunk");
        let mut header = gptcore::GptHeader::parse(&chunk.data).expect("parse");
        header.first_usable_lba = 0;
        chunk.data = header.to_block(512, &CRC);
    }
    let chunk =
        archive.chunks.iter_mut().find(|c| c.role == Role::MainEntries).expect("main array");
    chunk.lba = 5000;

    img.zero_lba(1, 1);
    img.zero_lba(img.last_block(), 1);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let err = restore_plan(&archive, &analysis).expect_err("nothing can vouch for LBA 5000");
    match err {
        Mismatch::UnvouchedEntries { role: Role::MainEntries, lba: 5000, .. } => {}
        other => panic!("expected an unvouched refusal, got {other:?}"),
    }
    let text = format!("{err}");
    assert!(text.contains("can say where the partitions are"), "{text}");
    assert!(!text.contains("would land inside"), "{text}");
}

/// An enlarged partition table is a supported `sgdisk -S 256` away, and its
/// array is 64 blocks rather than the conventional 32. The disk it came
/// from is the one that can vet that, and on the day it is needed the disk
/// is exactly what has stopped being able to: refusing then is refusing a
/// good snapshot of the right disk in the only situation it exists for.
#[test]
fn an_enlarged_table_restores_onto_a_disk_with_no_readable_table() {
    let img = common::resized_table_image();
    let before = img.read_lba(0, 66);
    let before_print = img.print();
    let archive = snapshot(&img);
    let main = archive.chunk(Role::MainEntries).expect("main array");
    assert_eq!((main.lba, main.data.len() / 512), (2, 64), "the fixture is not an enlarged table");

    img.zero_lba(1, 1);
    img.zero_lba(img.last_block(), 1);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("the snapshot's own headers vouch for it");
    apply(&mut disk, &plan).expect("apply");

    assert_eq!(img.read_lba(0, 66), before, "restore was not byte-exact");
    assert_eq!(img.print(), before_print);
    assert!(img.is_clean(), "{}", img.verify());
}

/// And the real corruption's array at 2016, which is neither at LBA 2 nor
/// derivable from anything left on a wrecked disk. Its own headers put
/// first usable at 2048, so it is recorded and restored from the gap.
#[test]
fn the_real_corruptions_snapshot_restores_onto_a_disk_with_no_readable_table() {
    let img = deck_corrupt_image();
    let archive = snapshot(&img);
    assert_eq!(archive.chunk(Role::MainEntries).unwrap().lba, 2016);

    img.zero_lba(1, 1);
    img.zero_lba(img.last_block(), 1);
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let plan = restore_plan(&archive, &analysis).expect("evidence stays restorable");
    apply(&mut disk, &plan).expect("apply");

    // Put back as found: the secondary is sound, the main still points at
    // 2016, and repair is what fixes that.
    let mut disk = img.disk();
    let after = analyze(&mut disk, &CRC).expect("analyze");
    assert_eq!(after.verdict, gptcore::Verdict::MainRepairable);
}

/// A header is worth nothing without the array it names. Capture drops a
/// chunk it could not read, and installing the header anyway would leave
/// the table pointing at whatever those blocks happen to hold — both
/// tables invalid where the disk may have had one good one, which is a
/// state this tool then refuses to write to at all.
#[test]
fn a_snapshot_missing_its_entry_array_is_refused() {
    for role in [Role::MainEntries, Role::SecondaryEntries] {
        let img = deck_image();
        let mut archive = snapshot(&img);
        archive.chunks.retain(|c| c.role != role);

        assert!(
            !backup::tables_verify(&archive, &CRC),
            "a header with no array under it must not verify as sound"
        );

        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        match restore_plan(&archive, &analysis) {
            Err(Mismatch::MissingEntries { role: got }) if got == role => {}
            other => panic!("expected a missing-array refusal for {role:?}, got {other:?}"),
        }
    }
}

/// Most of an array is not the array either. The header's CRC covers the
/// tail as much as the head, so a chunk that stops short leaves exactly
/// the state a missing one would: a table naming blocks the restore never
/// wrote.
#[test]
fn a_snapshot_whose_entry_array_stops_short_is_refused() {
    for role in [Role::MainEntries, Role::SecondaryEntries] {
        let img = deck_image();
        let mut archive = snapshot(&img);
        let chunk = archive.chunks.iter_mut().find(|c| c.role == role).expect("array chunk");
        chunk.data.truncate(chunk.data.len() - 512);

        assert!(
            !backup::tables_verify(&archive, &CRC),
            "a header over a short array must not verify as sound"
        );

        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        match restore_plan(&archive, &analysis) {
            Err(Mismatch::ShortEntries { role: got, have: 15872, need: 16384 }) if got == role => {}
            other => panic!("expected a short-array refusal for {role:?}, got {other:?}"),
        }
    }
}

/// And the shape capture itself can write: an enlarged table whose header
/// aims its array into a partition. The pointer is refused and the array
/// taken from the conventional place at the conventional size, while the
/// header — which names twice that much — is recorded as found. The file
/// that comes out is not restorable, and saying so is this end's job.
#[test]
fn a_captured_short_array_under_an_enlarged_header_is_refused() {
    let img = common::resized_table_image();
    let mut header = gptcore::GptHeader::parse(&img.read_lba(1, 1)).expect("parse");
    header.partition_entry_lba = 1_000_000;
    img.write_lba(1, &header.to_block(512, &CRC));

    let archive = snapshot(&img);
    let main = archive.chunk(Role::MainEntries).expect("main array");
    assert_eq!(
        (main.lba, main.data.len()),
        (2, 16384),
        "the pointer must not be followed, and the fallback is the conventional size"
    );

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    match restore_plan(&archive, &analysis) {
        Err(Mismatch::ShortEntries { role: Role::MainEntries, have: 16384, need: 32768 }) => {}
        other => panic!("expected a short-array refusal, got {other:?}"),
    }
}

/// Fixing where a structural chunk starts says nothing about where it
/// ends. An oversized protective MBR is written from LBA 0 forward, over
/// the entry array the same plan flushed two steps earlier and into the
/// partitions past it.
#[test]
fn an_oversized_structural_chunk_is_refused() {
    for role in [Role::Mbr, Role::MainHeader] {
        let img = deck_image();
        let mut archive = snapshot(&img);
        let chunk = archive.chunks.iter_mut().find(|c| c.role == role).expect("chunk");
        chunk.data.resize(8000 * 512, 0xCC);

        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        match restore_plan(&archive, &analysis) {
            Err(Mismatch::WrongSize { role: got, blocks: 8000 }) if got == role => {}
            other => panic!("expected a size refusal for {role:?}, got {other:?}"),
        }
    }
}

/// The real corruption's array lives at 2016 — inside the gap below the
/// first usable block. That snapshot is evidence, and it stays both
/// capturable from there (asserted above) and restorable to there.
#[test]
fn the_real_corruptions_snapshot_is_still_restorable() {
    let img = deck_corrupt_image();
    let archive = snapshot(&img);
    assert_eq!(archive.chunk(Role::MainEntries).unwrap().lba, 2016);

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    restore_plan(&archive, &analysis).expect("the 2016 array sits clear of every partition");
}

/// The "this came from a damaged table" warning is the operator's one clue
/// that a snapshot is not what it looks like, and it hung on a single byte
/// in the file. A snapshot's own bytes are a better witness than its own
/// claim about them.
#[test]
fn a_snapshots_health_is_checked_rather_than_read_off_the_file() {
    let img = deck_image();
    let healthy = snapshot(&img);
    assert_eq!(healthy.health, Health::Healthy);
    assert!(backup::tables_verify(&healthy, &CRC), "a real snapshot of a healthy disk verifies");

    // Damage the table inside the file and claim it was fine.
    let mut lying = snapshot(&img);
    let chunk = lying.chunks.iter_mut().find(|c| c.role == Role::MainEntries).expect("main array");
    chunk.data[0] ^= 0xff;
    lying.health = Health::Healthy;
    assert!(!backup::tables_verify(&lying, &CRC), "the entry array no longer matches its CRC");
}

/// Two tables can each verify and still describe different disks. That is
/// the state a planted secondary GPT sits in, unremarked, until the main
/// table breaks and it becomes the source a repair copies from. sgdisk
/// says "main and backup partition tables differ"; so should this.
#[test]
fn two_tables_that_disagree_are_reported_even_though_both_verify() {
    let img = deck_image();
    {
        let mut disk = img.disk();
        let analysis = analyze(&mut disk, &CRC).expect("analyze");
        assert_eq!(gptcore::repair::tables_differ(&analysis), Some(false), "a real disk agrees");
    }

    // Shift every extent in the secondary's array and reseal both CRCs, so
    // the secondary is valid in isolation and contradicts the main table.
    let last = img.last_block();
    let mut header = gptcore::GptHeader::parse(&img.read_lba(last, 1)).expect("parse");
    let mut entries = img.read_lba(header.partition_entry_lba, 32);
    for i in 0..128 {
        let e = i * 128;
        let start = u64::from_le_bytes(entries[e + 32..e + 40].try_into().unwrap());
        if start == 0 {
            continue;
        }
        let end = u64::from_le_bytes(entries[e + 40..e + 48].try_into().unwrap());
        entries[e + 32..e + 40].copy_from_slice(&(start + 40_000_000).to_le_bytes());
        entries[e + 40..e + 48].copy_from_slice(&(end + 40_000_000).to_le_bytes());
    }
    header.partition_entry_array_crc32 = gptcore::Crc32::crc32(&CRC, &entries);
    img.write_lba(header.partition_entry_lba, &entries);
    img.write_lba(last, &header.to_block(512, &CRC));

    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    assert!(analysis.main.as_ref().is_ok_and(|t| t.is_valid()), "the main table still verifies");
    assert!(analysis.secondary.as_ref().is_ok_and(|t| t.is_valid()), "and so does the secondary");
    assert_eq!(gptcore::repair::tables_differ(&analysis), Some(true));

    let text = gptcore::style::plain(&gptcore::report::render_analysis(&analysis));
    assert!(text.contains("describe different layouts"), "{text}");
}
