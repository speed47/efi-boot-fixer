//! Tests against real sectors from a dual-booting Steam Deck.
//!
//! The synthetic images in `repair_images.rs` were built from my reading of
//! the documented SteamOS layout, and got the partition type GUIDs wrong for
//! seven of the eight partitions. This file exists so that the layout the
//! tool is checked against is the one that is actually on the hardware.

mod common;

use common::deck_image;
use gptcore::crc::SoftCrc32;
use gptcore::layout::{self, Confidence};
use gptcore::repair::{analyze, apply, plan, Verdict};

const CRC: SoftCrc32 = SoftCrc32;

/// The real disk: 1 TB NVMe, 512-byte sectors, GPT written by util-linux
/// fdisk, so the entry array ends at LBA 33 but the first usable block is
/// 2048 rather than 34.
#[test]
fn fixture_reads_back_as_a_healthy_disk() {
    let img = deck_image();
    assert!(img.is_clean(), "fixture should be healthy:\n{}", img.verify());

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.verdict, Verdict::Healthy);
    assert!(dev.writes().is_empty());

    let primary = analysis.primary.as_ref().unwrap();
    assert_eq!(primary.header.first_usable_lba, 2048);
    assert_eq!(primary.header.last_usable_lba, 1_953_525_134);
    assert_eq!(primary.header.number_of_partition_entries, 128);
    assert_eq!(primary.header.size_of_partition_entry, 128);
    assert_eq!(analysis.last_block, 1_953_525_167);
}

/// The gap between the entry array (ending at LBA 33) and the first usable
/// block (2048) is unusual enough that sgdisk warns about it, and none of
/// the synthetic fixtures have it.
#[test]
fn entry_array_gap_before_first_usable_is_accepted() {
    let img = deck_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let header = &analysis.primary.as_ref().unwrap().header;
    let blocks = header.entry_array_blocks(512).unwrap();
    assert_eq!(blocks, 32);
    assert!(2 + blocks < header.first_usable_lba, "expected a gap, not a tight fit");
}

/// The check that matters: with the primary destroyed, is the real backup
/// accepted as a repair source?
#[test]
fn real_deck_backup_is_accepted_as_a_repair_source() {
    let img = deck_image();
    img.zero_lba(1, 1);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(
        analysis.verdict,
        Verdict::PrimaryRepairable,
        "refused the real disk: {:?}",
        analysis.rejection
    );

    let rec = analysis.recognition.as_ref().expect("assessed");
    assert_eq!(rec.confidence, Confidence::SteamOs, "missing: {:?}", rec.missing);
    assert!(rec.missing_critical.is_empty());
    assert!(rec.unknown_types.is_empty(), "unrecognised types: {:?}", rec.unknown_types);
}

/// End to end on the real layout, judged by sgdisk.
#[test]
fn real_deck_primary_is_repaired() {
    let img = deck_image();
    let before = img.print();

    img.zero_lba(1, 1);
    assert!(!img.is_clean());

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let repair = plan(&analysis, &CRC).expect("a plan");
    assert_eq!(repair.header.my_lba, 1);
    assert_eq!(repair.header.alternate_lba, 1_953_525_167);
    assert_eq!(repair.header.partition_entry_lba, 2);
    apply(&mut dev, &repair).unwrap();
    drop(dev);

    assert!(img.is_clean(), "sgdisk unhappy after repair:\n{}", img.verify());
    assert_eq!(before, img.print(), "table changed");
}

/// The protective MBR on the real disk is already canonical, so the tool
/// must leave LBA 0 alone.
#[test]
fn real_protective_mbr_needs_no_repair() {
    let img = deck_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(analysis.mbr, gptcore::mbr::MbrStatus::Protective);
    assert!(!analysis.mbr.needs_repair());

    // And what we would generate matches what is already there.
    let regenerated = gptcore::mbr::generate(Some(&analysis.mbr_raw), 512, analysis.last_block);
    assert_eq!(regenerated[440..512], analysis.mbr_raw[440..512], "MBR record differs");
}

/// Every partition type on the disk should be nameable, or the report
/// tells the operator "unknown" about a perfectly normal Deck.
#[test]
fn every_type_guid_on_the_real_disk_is_known() {
    let img = deck_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let table = analysis.primary.as_ref().unwrap();

    let mut unknown = Vec::new();
    for (i, e) in table.used_entries() {
        if layout::describe_type(&e.type_guid) == "unknown" {
            unknown.push((i + 1, e.type_guid, e.name_string()));
        }
    }
    assert!(unknown.is_empty(), "unnamed partition types: {unknown:?}");
}

// ---------------------------------------------------------------- prevention

/// The real disk has FirstUsableLBA 2048 with the entry array ending at 33,
/// which is exactly the gap the Windows corruption's arithmetic trips over.
#[test]
fn the_real_deck_is_a_candidate_for_closing_the_gap() {
    let img = deck_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(
        gptcore::prevent::assess(&analysis),
        gptcore::prevent::Verdict::Applicable { current: 2048, proposed: 34 }
    );
}

#[test]
fn closing_the_gap_leaves_the_disk_healthy_and_the_table_unchanged() {
    let img = deck_image();
    let before = img.print();

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let plan = gptcore::prevent::plan(&analysis, &CRC).expect("a plan");

    // Only the two header blocks; the entry arrays are already correct.
    let touched: Vec<u64> = plan.writes().map(|(lba, _)| lba).collect();
    assert_eq!(touched, vec![1, 1_953_525_167]);

    apply(&mut dev, &plan).unwrap();
    drop(dev);

    assert!(img.is_clean(), "sgdisk unhappy:\n{}", img.verify());
    assert_eq!(before, img.print(), "partitions must not move");

    let mut dev = img.disk();
    let after = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(after.verdict, Verdict::Healthy);
    assert_eq!(after.primary.as_ref().unwrap().header.first_usable_lba, 34);
    assert_eq!(after.backup.as_ref().unwrap().header.first_usable_lba, 34);
}

#[test]
fn closing_the_gap_is_idempotent() {
    let img = deck_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    apply(&mut dev, &gptcore::prevent::plan(&analysis, &CRC).unwrap()).unwrap();
    drop(dev);

    let mut dev = img.disk();
    let second = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(
        gptcore::prevent::assess(&second),
        gptcore::prevent::Verdict::AlreadyMinimal { current: 34 }
    );
    assert!(gptcore::prevent::plan(&second, &CRC).is_none());
}

/// The hybrid-MBR refusal is unconditional, and this operation is the one
/// place it could have leaked.
///
/// Nothing about a hybrid MBR makes either GPT invalid, so every check
/// prevention makes would pass: the tables are healthy, the entry array is
/// at LBA 2, and the real Deck has a gap to close. Repair refuses such a
/// disk on `analysis.verdict`, which prevention has no reason to consult —
/// so without an explicit check this would rewrite both headers on a disk
/// the tool has promised not to touch.
#[test]
fn a_hybrid_mbr_is_refused_for_prevention_though_both_gpts_are_healthy() {
    let img = deck_image();
    let mut mbr = img.read_lba(0, 1);
    // A real NTFS record beside the protective one, as in the repair test.
    let rec = 446 + 16;
    mbr[rec] = 0x80;
    mbr[rec + 4] = 0x07;
    mbr[rec + 8..rec + 12].copy_from_slice(&2048u32.to_le_bytes());
    mbr[rec + 12..rec + 16].copy_from_slice(&524_288u32.to_le_bytes());
    img.write_lba(0, &mbr);

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();

    // The precondition that makes this test worth having: both tables are
    // fine, so nothing but the MBR check stands between us and a write.
    assert!(analysis.primary.as_ref().unwrap().is_valid());
    assert!(analysis.backup.as_ref().unwrap().is_valid());

    assert_eq!(
        gptcore::prevent::assess(&analysis),
        gptcore::prevent::Verdict::Refused(gptcore::prevent::Blocker::HybridMbr)
    );
    assert!(gptcore::prevent::plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty());
}

/// A disk whose primary is broken must be repaired first; this operation
/// rewrites healthy headers and has no business guessing at a damaged one.
#[test]
fn a_damaged_disk_is_refused_for_prevention() {
    let img = deck_image();
    img.zero_lba(1, 1);
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(
        gptcore::prevent::assess(&analysis),
        gptcore::prevent::Verdict::Refused(gptcore::prevent::Blocker::TableNotHealthy)
    );
    assert!(gptcore::prevent::plan(&analysis, &CRC).is_none());
    assert!(dev.writes().is_empty());
}
