//! What the NVRAM write paths will do, before they are allowed to do it.
//!
//! The plans are ordered lists of variable writes rather than a function
//! that writes as it goes, for the same reason `repair::plan` is: the
//! ordering is the safety property, and an ordering you can assert on is
//! worth more than a comment saying what order the code happens to run in.

use gptcore::backup::Timestamp;
use gptcore::bootcfg::{self, Snapshot};
use gptcore::bootopt::{self, LoadOption, LOAD_OPTION_ACTIVE};
use gptcore::{Crc32, SoftCrc32};

const CRC: SoftCrc32 = SoftCrc32;

fn entry(description: &str) -> LoadOption {
    LoadOption {
        attributes: LOAD_OPTION_ACTIVE,
        description: description.into(),
        device_path: include_bytes!("data/boot/ovmf-boot0001.bin")[24..].to_vec(),
        optional_data: Vec::new(),
    }
}

// ------------------------------------------------------------- registering

/// The invariant, stated as an assertion rather than a comment: the entry
/// is on the medium before anything points at it.
#[test]
fn the_entry_is_written_before_the_order_that_names_it() {
    let writes = bootopt::plan_register(4, &entry("SteamOS"), &[0, 1], false);
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].name, "Boot0004");
    assert_eq!(writes[1].name, "BootOrder");
}

#[test]
fn registering_appends_to_the_order_by_default() {
    let writes = bootopt::plan_register(4, &entry("SteamOS"), &[0, 1], false);
    assert_eq!(bootopt::decode_order(&writes[1].data).unwrap(), vec![0, 1, 4]);
}

#[test]
fn registering_first_makes_it_the_default_in_one_operation() {
    let writes = bootopt::plan_register(4, &entry("SteamOS"), &[0, 1], true);
    assert_eq!(bootopt::decode_order(&writes[1].data).unwrap(), vec![4, 0, 1]);
}

/// Re-registering a slot already in the order must move it, not list it
/// twice. A duplicated slot is an order the firmware walks over twice.
#[test]
fn registering_a_slot_already_in_the_order_does_not_duplicate_it() {
    let writes = bootopt::plan_register(1, &entry("SteamOS"), &[0, 1, 2], true);
    assert_eq!(bootopt::decode_order(&writes[1].data).unwrap(), vec![1, 0, 2]);
}

#[test]
fn the_entry_written_is_the_entry_that_decodes_back() {
    let opt = entry("SteamOS");
    let writes = bootopt::plan_register(4, &opt, &[], false);
    assert_eq!(bootopt::decode(&writes[0].data).unwrap(), opt);
}

// ---------------------------------------------------------- setting default

#[test]
fn setting_the_default_only_touches_the_order() {
    let writes = bootopt::plan_set_default(2, &[0, 1, 2, 3]);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].name, "BootOrder");
    assert_eq!(bootopt::decode_order(&writes[0].data).unwrap(), vec![2, 0, 1, 3]);
}

#[test]
fn setting_a_default_that_is_not_in_the_order_yet_adds_it() {
    assert_eq!(bootopt::reorder(9, &[0, 1]), vec![9, 0, 1]);
}

#[test]
fn setting_the_current_default_again_changes_nothing() {
    assert_eq!(bootopt::reorder(0, &[0, 1, 2]), vec![0, 1, 2]);
}

// ------------------------------------------------------------- boot next

#[test]
fn boot_next_is_one_little_endian_slot_and_nothing_else() {
    let writes = bootopt::plan_boot_next(0x1234);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].name, "BootNext");
    assert_eq!(writes[0].data, vec![0x34, 0x12]);
}

/// Every plan has to say what it will write, or the confirmation screen
/// has nothing to show.
#[test]
fn every_plan_renders_the_variables_it_will_write() {
    for writes in [
        bootopt::plan_register(4, &entry("SteamOS"), &[0], true),
        bootopt::plan_set_default(1, &[0, 1]),
        bootopt::plan_boot_next(1),
    ] {
        let text = gptcore::style::plain(&bootopt::render_plan(&writes));
        for w in &writes {
            assert!(text.contains(&w.name), "{} missing from:\n{text}", w.name);
        }
    }
}

#[test]
fn the_order_change_shows_the_result_not_the_edit() {
    let before = vec![0, 1, 2];
    let after = bootopt::reorder(2, &before);
    let text =
        gptcore::style::plain(&bootopt::render_order_change(&before, &after, bootopt::slot_name));
    assert!(text.contains("Boot0002"), "{text}");
    assert!(text.contains("Boot order after:"), "{text}");
}

// ------------------------------------------------------------- snapshots

fn snapshot() -> Snapshot {
    Snapshot {
        time: Timestamp { year: 2026, month: 8, day: 2, hour: 13, minute: 30, second: 5 },
        vars: vec![
            (String::from("BootOrder"), vec![0x00, 0x00, 0x01, 0x00]),
            (String::from("Boot0000"), include_bytes!("data/boot/ovmf-boot0000.bin").to_vec()),
            (String::from("Boot0001"), include_bytes!("data/boot/ovmf-boot0001.bin").to_vec()),
            (String::from("Timeout"), vec![0x03, 0x00]),
        ],
        meta: vec![(String::from("tool"), String::from("gpttoolk 0.1.0"))],
    }
}

#[test]
fn a_snapshot_round_trips() {
    let snap = snapshot();
    assert_eq!(bootcfg::decode(&bootcfg::encode(&snap, &CRC), &CRC).unwrap(), snap);
}

/// The reason variables are stored as opaque bytes: a `Boot####` this
/// build cannot parse is precisely the one worth copying exactly.
#[test]
fn a_variable_that_will_not_decode_is_still_saved_verbatim() {
    let mut snap = snapshot();
    let rubbish = vec![0xff; 7];
    snap.vars.push((String::from("Boot0009"), rubbish.clone()));
    assert!(bootopt::decode(&rubbish).is_err());

    let back = bootcfg::decode(&bootcfg::encode(&snap, &CRC), &CRC).unwrap();
    assert_eq!(back.get("Boot0009"), Some(rubbish.as_slice()));
}

#[test]
fn an_empty_snapshot_is_still_a_valid_file() {
    let snap = Snapshot { time: Timestamp::default(), vars: Vec::new(), meta: Vec::new() };
    assert_eq!(bootcfg::decode(&bootcfg::encode(&snap, &CRC), &CRC).unwrap(), snap);
}

#[test]
fn entries_are_counted_apart_from_settings() {
    assert_eq!(snapshot().entry_count(), 2);
}

#[test]
fn a_damaged_snapshot_is_refused_rather_than_half_read() {
    let good = bootcfg::encode(&snapshot(), &CRC);

    let mut flipped = good.clone();
    let last = flipped.len() - 8;
    flipped[last] ^= 0xff;
    assert!(matches!(
        bootcfg::decode(&flipped, &CRC),
        Err(bootcfg::DecodeError::BadChecksum { .. })
    ));

    for cut in [0, 1, FIXED_ISH, good.len() - 5] {
        assert!(bootcfg::decode(&good[..cut], &CRC).is_err(), "{cut} bytes was accepted");
    }

    let mut wrong_magic = good.clone();
    wrong_magic[0] = b'X';
    assert_eq!(bootcfg::decode(&wrong_magic, &CRC), Err(bootcfg::DecodeError::BadMagic));
}

/// Long enough to clear the fixed head, short enough to cut a variable in
/// half.
const FIXED_ISH: usize = 30;

/// A file from a later build must be refused, not misread.
#[test]
fn a_later_version_is_refused() {
    let mut bytes = bootcfg::encode(&snapshot(), &CRC);
    bytes[8] = 99;
    let end = bytes.len() - 4;
    let sum = CRC.crc32(&bytes[..end]);
    bytes[end..].copy_from_slice(&sum.to_le_bytes());
    assert_eq!(
        bootcfg::decode(&bytes, &CRC),
        Err(bootcfg::DecodeError::UnsupportedVersion { found: 99 })
    );
}

#[test]
fn snapshot_names_count_up_and_never_fill_a_gap() {
    assert_eq!(bootcfg::next_name(&[]).unwrap(), "boot.001");
    assert_eq!(bootcfg::next_name(&[String::from("boot.001")]).unwrap(), "boot.002");
    // The gap at 002 stays a gap, unlike a boot slot.
    let taken = vec![String::from("boot.001"), String::from("boot.003")];
    assert_eq!(bootcfg::next_name(&taken).unwrap(), "boot.004");
    // FAT may hand the name back upper-cased.
    assert_eq!(bootcfg::next_name(&[String::from("BOOT.007")]).unwrap(), "boot.008");
    // GPT snapshots share the directory and must not be counted.
    assert_eq!(bootcfg::next_name(&[String::from("gpt.050")]).unwrap(), "boot.001");
}

#[test]
fn the_snapshot_space_can_run_out_rather_than_wrap() {
    let taken = vec![format!("boot.{}", bootcfg::MAX_SEQUENCE)];
    assert_eq!(bootcfg::next_name(&taken), None);
}

/// A snapshot the running tool wrote to an ESP under OVMF, recovered from
/// the FAT image afterwards.
///
/// Worth more than the hand-built one above for the same reason the OVMF
/// variables are worth more than hand-built entries: nothing in this test
/// process produced it. It also pins the thing most easily broken by a
/// later edit — that the copy is taken *before* the change, not after. The
/// run that wrote this went on to add `Boot0004`, and the `BootOrder`
/// preserved here has four slots, not five.
#[test]
fn decodes_a_snapshot_written_by_the_tool_under_firmware() {
    let bytes = include_bytes!("data/boot/esp-boot-snapshot.bin");
    let snap = bootcfg::decode(bytes, &CRC).expect("the tool writes readable snapshots");

    assert_eq!(snap.time.year, 2026);
    assert_eq!(snap.entry_count(), 4);
    assert_eq!(
        snap.get("BootOrder"),
        Some([0u16, 1, 2, 3].map(u16::to_le_bytes).concat().as_slice())
    );

    // Entries are stored verbatim: this is OVMF's UiApp, byte for byte the
    // same as the copy captured straight out of the variable store.
    assert_eq!(
        snap.get("Boot0000"),
        Some(include_bytes!("data/boot/ovmf-boot0000.bin").as_slice())
    );

    // Provenance survived, including which firmware it came off.
    let meta: Vec<&str> = snap.meta.iter().map(|(k, _)| k.as_str()).collect();
    assert!(meta.contains(&"tool"), "{meta:?}");
    assert!(meta.contains(&"firmware"), "{meta:?}");

    assert_eq!(bootcfg::encode(&snap, &CRC), bytes);
}

#[test]
fn describing_a_snapshot_names_every_variable_in_it() {
    let snap = snapshot();
    let text = gptcore::style::plain(&bootcfg::describe(&snap));
    for (name, _) in &snap.vars {
        assert!(text.contains(name.as_str()), "{name} missing from:\n{text}");
    }
}
