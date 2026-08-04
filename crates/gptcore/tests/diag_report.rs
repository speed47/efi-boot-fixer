//! The disk half of the diagnostic report, rendered from real sectors.
//!
//! The report is written to be pasted into a forum post, which means the
//! things that matter about it are not "does it compile" but "is the value
//! somebody will be asked for actually in there". So these assert on
//! content: the disk GUID, both headers' fields, the MBR records as
//! numbers, and every partition's own GUID.

mod common;

use common::{deck_corrupt_image, deck_image};
use gptcore::crc::SoftCrc32;
use gptcore::diag;
use gptcore::repair::analyze;
use gptcore::style::plain;

const CRC: SoftCrc32 = SoftCrc32;

fn report(image: &common::Image) -> String {
    let mut dev = image.disk();
    let analysis = analyze(&mut dev, &CRC).expect("analyze");
    plain(&diag::render_disk(&analysis))
}

#[test]
fn a_healthy_deck_reports_every_header_field() {
    let text = report(&deck_image());
    for wanted in [
        "signature",
        "\"EFI PART\"",
        "my LBA",
        "alternate LBA",
        "first usable LBA  : 2048",
        "last usable LBA   : 1953525134",
        "entry array       : 128 entries x 128 bytes",
        "Main GPT header:",
        "Secondary GPT header:",
        "defects           : none",
    ] {
        assert!(text.contains(wanted), "report has no {wanted:?}:\n{text}");
    }
}

/// The four records are printed whether or not the MBR is healthy: it is
/// the evidence behind the word "protective", and the only place a hybrid
/// MBR can be seen rather than merely asserted.
#[test]
fn the_protective_mbr_is_printed_as_numbers() {
    let text = report(&deck_image());
    assert!(text.contains("MBR partition records at LBA 0:"), "{text}");
    assert!(text.contains("0xEE"), "no protective record in:\n{text}");
    // Three unused slots follow the one protective record.
    assert_eq!(text.matches("(unused)").count(), 3, "{text}");
}

/// Every partition carries two GUIDs, and the unique one is what identifies
/// this machine's disk in somebody else's thread.
#[test]
fn partitions_carry_both_guids_and_their_attributes() {
    let text = report(&deck_image());
    assert!(text.contains("Partitions, as the main GPT has them:"), "{text}");
    assert!(text.contains("\"esp\""), "{text}");
    assert!(text.contains("EFI system partition"), "{text}");
    assert!(text.contains("type   C12A7328-F81F-11D2-BA4B-00A0C93EC93B"), "{text}");
    assert!(text.contains("unique "), "{text}");
    assert!(text.contains("attrs  0x"), "{text}");
}

/// A healthy disk's two tables agree, and saying so once beats printing
/// forty identical lines a second time.
#[test]
fn identical_tables_are_stated_rather_than_repeated() {
    let text = report(&deck_image());
    assert!(text.contains("The secondary GPT lists exactly the same partitions."), "{text}");
    assert_eq!(text.matches("Partitions, as").count(), 1, "{text}");
}

/// The real corruption: a main header whose CRC is valid but which points
/// its entry array at the wrong LBA. The report has to show the defect and
/// the field it was derived from, or it proves nothing to a reader.
#[test]
fn a_wrecked_main_gpt_shows_the_defect_and_the_field_behind_it() {
    let text = report(&deck_corrupt_image());
    assert!(text.contains("defect"), "no defect reported:\n{text}");
    assert!(text.contains("entry array LBA   : 2016"), "{text}");
    // The secondary is intact and still points at its own array, so the
    // two headers must not read alike.
    assert!(text.contains("Secondary GPT header:"), "{text}");
}
