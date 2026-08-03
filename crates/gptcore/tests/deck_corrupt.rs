//! The failure this tool was written for, as it actually occurred.
//!
//! Every corruption in `repair_images.rs` was invented: zeroed headers,
//! flipped CRCs, trashed entry arrays. The real one is none of those. It is
//! a two-byte change:
//!
//! ```text
//! PartitionEntryLBA:  2  ->  2016      (= FirstUsableLBA - 32)
//! HeaderCRC32:               recomputed to match
//! ```
//!
//! The header is otherwise perfect and its CRC *passes*. Something wrote a
//! spec-violating entry-array location and then correctly resealed the
//! header, so the only things standing between this disk and a "healthy"
//! verdict are the partition-entry-array CRC and the explicit check that a
//! main GPT header points at LBA 2.
//!
//! The fixtures are the real sectors with the disk GUID and every unique
//! partition GUID replaced by placeholders and the CRCs resealed, which
//! preserves that pathology exactly.

mod common;

use common::{deck_corrupt_image, deck_expected_fixed_head, BLOCK_SIZE};
use gptcore::crc::SoftCrc32;
use gptcore::header::Defect;
use gptcore::mbr::MbrStatus;
use gptcore::repair::{analyze, apply, plan, Verdict};

const CRC: SoftCrc32 = SoftCrc32;

#[test]
fn the_header_crc_is_valid_so_only_two_checks_catch_this() {
    let img = deck_corrupt_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let main = analysis.main.as_ref().unwrap();

    assert!(
        !main.defects.iter().any(|d| matches!(d, Defect::HeaderCrcMismatch { .. })),
        "the real corruption has a VALID header CRC; a checker that only \
         verifies that would call this disk healthy: {:?}",
        main.defects
    );
    assert!(
        main.defects.iter().any(|d| matches!(d, Defect::MainEntryLbaNotTwo { found: 2016 })),
        "{:?}",
        main.defects
    );
    assert!(
        main.defects.iter().any(|d| matches!(d, Defect::EntryArrayCrcMismatch { .. })),
        "{:?}",
        main.defects
    );
    assert!(!main.is_valid());
}

/// Everything else about the disk was intact, including the protective MBR
/// and the secondary GPT, so the tool must not touch them.
#[test]
fn only_the_main_gpt_was_damaged() {
    let img = deck_corrupt_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();

    assert_eq!(analysis.mbr, MbrStatus::Protective);
    assert!(!analysis.mbr.needs_repair());
    assert!(analysis.secondary.as_ref().unwrap().is_valid());
    assert_eq!(analysis.verdict, Verdict::MainRepairable, "{:?}", analysis.rejection);

    let repair = plan(&analysis, &CRC).unwrap();
    let touched: Vec<u64> = repair.writes().map(|(lba, _)| lba).collect();
    assert_eq!(touched, vec![2, 1], "should write only the entry array then the header");
}

/// The strongest assertion available: our repair against an independent
/// implementation's, on the same bytes.
#[test]
fn repair_matches_gdisk_byte_for_byte() {
    let img = deck_corrupt_image();
    assert!(!img.is_clean(), "fixture should start broken");

    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    let repair = plan(&analysis, &CRC).expect("a plan");
    apply(&mut dev, &repair).unwrap();
    drop(dev);

    let expected = deck_expected_fixed_head();
    let got = img.read_lba(0, 34);
    for lba in 0..34usize {
        let lo = lba * BLOCK_SIZE as usize;
        let hi = lo + BLOCK_SIZE as usize;
        assert_eq!(
            &got[lo..hi],
            &expected[lo..hi],
            "LBA {lba} differs from what gdisk wrote when it repaired this disk"
        );
    }

    assert!(img.is_clean(), "sgdisk unhappy after repair:\n{}", img.verify());
}

#[test]
fn repaired_disk_is_healthy_and_stays_healthy() {
    let img = deck_corrupt_image();
    let mut dev = img.disk();
    let analysis = analyze(&mut dev, &CRC).unwrap();
    apply(&mut dev, &plan(&analysis, &CRC).unwrap()).unwrap();
    drop(dev);

    let mut dev = img.disk();
    let second = analyze(&mut dev, &CRC).unwrap();
    assert_eq!(second.verdict, Verdict::Healthy);
    assert!(plan(&second, &CRC).is_none());
    assert_eq!(second.main.as_ref().unwrap().header.partition_entry_lba, 2);
    assert!(dev.writes().is_empty());
}
