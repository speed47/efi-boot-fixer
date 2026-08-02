//! `EFI_LOAD_OPTION` round-trips and the ways a variable can lie.
//!
//! There is no independent oracle here the way `sgdisk` is one for the GPT
//! tests: `efibootmgr` is a Linux tool that needs `efivarfs`, so it cannot
//! judge a byte string on a build machine. The substitute is a fixture
//! assembled field by field from the spec (§3.1.3) with every offset
//! spelled out, so a reader can check it against the document rather than
//! against this crate's own opinion. `tests/data/ovmf-boot0000.bin` is the
//! other half: a variable this code never produced, captured from firmware.

use gptcore::bootopt::{
    self, DecodeError, LoadOption, LOAD_OPTION_ACTIVE, LOAD_OPTION_CATEGORY_APP, LOAD_OPTION_HIDDEN,
};
use gptcore::style::Style;

/// A GPT hard-drive node, built to the field layout in the spec.
fn hd_node() -> Vec<u8> {
    let mut v = Vec::new();
    v.push(0x04); // Media Device Path
    v.push(0x01); // Hard Drive
    v.extend_from_slice(&42u16.to_le_bytes()); // Length, including this header
    v.extend_from_slice(&1u32.to_le_bytes()); // PartitionNumber
    v.extend_from_slice(&2048u64.to_le_bytes()); // PartitionStart
    v.extend_from_slice(&1048576u64.to_le_bytes()); // PartitionSize
    v.extend_from_slice(&[0xaa; 16]); // PartitionSignature
    v.push(0x02); // PartitionFormat: GPT
    v.push(0x02); // SignatureType: GUID
    assert_eq!(v.len(), 42);
    v
}

/// A media file-path node holding `path`.
fn file_node(path: &str) -> Vec<u8> {
    let units: Vec<u16> = path.encode_utf16().chain(core::iter::once(0)).collect();
    let len = 4 + units.len() * 2;
    let mut v = Vec::new();
    v.push(0x04); // Media Device Path
    v.push(0x04); // File Path
    v.extend_from_slice(&(len as u16).to_le_bytes());
    for u in units {
        v.extend_from_slice(&u.to_le_bytes());
    }
    assert_eq!(v.len(), len);
    v
}

fn end_node() -> Vec<u8> {
    alloc_vec(&[0x7f, 0xff, 0x04, 0x00])
}

fn alloc_vec(b: &[u8]) -> Vec<u8> {
    b.to_vec()
}

fn device_path() -> Vec<u8> {
    let mut v = hd_node();
    v.extend_from_slice(&file_node("\\EFI\\steamos\\steamcl.efi"));
    v.extend_from_slice(&end_node());
    v
}

/// A whole `Boot####` variable, assembled by hand.
fn fixture(attributes: u32, description: &str, path: &[u8], optional: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&attributes.to_le_bytes());
    v.extend_from_slice(&(path.len() as u16).to_le_bytes());
    for u in description.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(path);
    v.extend_from_slice(optional);
    v
}

#[test]
fn decodes_each_field_from_its_declared_offset() {
    let path = device_path();
    let bytes = fixture(LOAD_OPTION_ACTIVE, "SteamOS", &path, &[]);

    // 4 attributes + 2 length + 8 UCS-2 units ("SteamOS" and its NUL) = 22.
    assert_eq!(bytes.len(), 22 + path.len());
    assert_eq!(&bytes[..4], &1u32.to_le_bytes());
    assert_eq!(&bytes[4..6], &(path.len() as u16).to_le_bytes());

    let opt = bootopt::decode(&bytes).expect("well-formed entry");
    assert_eq!(opt.attributes, LOAD_OPTION_ACTIVE);
    assert_eq!(opt.description, "SteamOS");
    assert_eq!(opt.device_path, path);
    assert!(opt.optional_data.is_empty());
    assert!(opt.is_active());
}

#[test]
fn round_trips_through_encode() {
    let opt = LoadOption {
        attributes: LOAD_OPTION_ACTIVE,
        description: "Windows Boot Manager".into(),
        device_path: device_path(),
        optional_data: vec![0x01, 0x02, 0x03],
    };
    assert_eq!(bootopt::decode(&bootopt::encode(&opt)).unwrap(), opt);
}

/// The direction that matters more: bytes this crate did not produce must
/// come back out unchanged, or writing an edited entry would corrupt it.
#[test]
fn re_encodes_foreign_bytes_byte_for_byte() {
    let bytes = fixture(LOAD_OPTION_ACTIVE, "SteamOS", &device_path(), &[0xde, 0xad]);
    let opt = bootopt::decode(&bytes).unwrap();
    assert_eq!(bootopt::encode(&opt), bytes);
}

/// OVMF's own `Boot0000`. Written by firmware, not by this crate, and
/// captured with `tools/dump-efivars.py`.
///
/// Worth having for two reasons beyond "it is real". Its attributes are
/// `0x109` — active, hidden and an application — which is the combination
/// the flag rendering has to get right, and its device path is a pair of
/// firmware-volume nodes with no hard-drive node at all. That last part is
/// the case `espscan` has to answer "matches nothing on any ESP" for
/// instead of guessing.
#[test]
fn decodes_ovmfs_own_ui_app_entry() {
    let bytes = include_bytes!("data/boot/ovmf-boot0000.bin");
    let opt = bootopt::decode(bytes).expect("firmware writes well-formed entries");

    assert_eq!(opt.attributes, 0x109);
    assert!(opt.is_active() && opt.is_hidden() && opt.is_app());
    assert_eq!(opt.description, "UiApp");
    assert_eq!(opt.device_path.len(), 44);
    // Media/PIWG-firmware-volume, not Media/hard-drive.
    assert_eq!(&opt.device_path[..2], &[0x04, 0x07]);
    assert!(opt.optional_data.is_empty());
    assert_eq!(bootopt::encode(&opt), bytes);
}

/// The same store's `Boot0001`: active with no other flags, and a
/// description long enough that the head offsets cannot be coincidence.
#[test]
fn decodes_ovmfs_shell_entry() {
    let bytes = include_bytes!("data/boot/ovmf-boot0001.bin");
    let opt = bootopt::decode(bytes).expect("firmware writes well-formed entries");

    assert_eq!(opt.attributes, LOAD_OPTION_ACTIVE);
    assert!(!opt.is_hidden() && !opt.is_app());
    assert_eq!(opt.description, "EFI Internal Shell");
    assert_eq!(opt.device_path.len(), 44);
    assert!(opt.optional_data.is_empty());
    assert_eq!(bootopt::encode(&opt), bytes);
}

#[test]
fn decodes_a_real_boot_order() {
    let bytes = include_bytes!("data/boot/ovmf-bootorder.bin");
    assert_eq!(bootopt::decode_order(bytes).unwrap(), vec![0x0000, 0x0001]);
    assert_eq!(bootopt::encode_order(&[0x0000, 0x0001]), bytes);
}

#[test]
fn an_empty_description_is_legal() {
    let path = device_path();
    let opt = bootopt::decode(&fixture(0, "", &path, &[])).unwrap();
    assert_eq!(opt.description, "");
    assert_eq!(opt.device_path, path);
}

#[test]
fn optional_data_is_whatever_follows_the_declared_path() {
    let path = device_path();
    let opt = bootopt::decode(&fixture(0, "x", &path, b"root=/dev/sda2")).unwrap();
    assert_eq!(opt.device_path, path);
    assert_eq!(opt.optional_data, b"root=/dev/sda2");
}

#[test]
fn rejects_a_buffer_too_short_for_the_head() {
    for len in 0..8 {
        let bytes = vec![0u8; len];
        match bootopt::decode(&bytes) {
            Err(DecodeError::TooShort { .. }) => {}
            // Eight bytes is a valid empty-description, empty-path entry.
            Ok(_) if len == 8 => {}
            other => panic!("{len} bytes gave {other:?}"),
        }
    }
}

#[test]
fn rejects_a_description_with_no_terminator() {
    let mut bytes = vec![0u8; 6];
    for u in "SteamOS".encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    assert_eq!(bootopt::decode(&bytes), Err(DecodeError::UnterminatedDescription));
}

/// A trailing odd byte cannot be half a terminator. The entry has to be
/// refused, not read up to it.
#[test]
fn rejects_an_odd_length_description_region() {
    let mut bytes = vec![0u8; 6];
    for u in "AB".encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes.push(0x00);
    assert_eq!(bootopt::decode(&bytes), Err(DecodeError::UnterminatedDescription));
}

/// The failure a variable store that ran out of room actually produces.
#[test]
fn rejects_a_path_length_that_overruns_the_buffer() {
    let path = device_path();
    let full = fixture(LOAD_OPTION_ACTIVE, "SteamOS", &path, &[]);
    let truncated = &full[..full.len() - 10];
    match bootopt::decode(truncated) {
        Err(DecodeError::PathOverruns { declared, available }) => {
            assert_eq!(declared, path.len());
            assert_eq!(available, path.len() - 10);
        }
        other => panic!("expected an overrun, got {other:?}"),
    }
}

#[test]
fn boot_order_round_trips() {
    let order = vec![0x0003, 0x0000, 0xffff];
    let bytes = bootopt::encode_order(&order);
    assert_eq!(bytes, vec![0x03, 0x00, 0x00, 0x00, 0xff, 0xff]);
    assert_eq!(bootopt::decode_order(&bytes).unwrap(), order);
    assert_eq!(bootopt::decode_order(&[]).unwrap(), Vec::<u16>::new());
}

#[test]
fn rejects_a_boot_order_that_is_not_whole_entries() {
    assert_eq!(bootopt::decode_order(&[0x00]), Err(DecodeError::OddOrderLength { len: 1 }));
    assert_eq!(
        bootopt::decode_order(&[0x00, 0x00, 0x01]),
        Err(DecodeError::OddOrderLength { len: 3 })
    );
}

#[test]
fn slot_names_round_trip() {
    for slot in [0u16, 1, 0x000f, 0x00a0, 0x1234, 0xffff] {
        assert_eq!(bootopt::parse_slot(&bootopt::slot_name(slot)), Some(slot));
    }
    assert_eq!(bootopt::slot_name(1), "Boot0001");
    assert_eq!(bootopt::slot_name(0xabcd), "BootABCD");
    // Lower case is accepted on the way in; the spec's own spelling is
    // upper, and that is what is written.
    assert_eq!(bootopt::parse_slot("Bootabcd"), Some(0xabcd));
}

/// The variable store is enumerated to find entries, so everything else
/// beginning "Boot" has to be rejected or it lands in the entry list.
#[test]
fn rejects_names_that_are_not_boot_slots() {
    for name in [
        "BootOrder",
        "BootNext",
        "BootCurrent",
        "BootOptionSupport",
        "Boot001",
        "Boot00001",
        "BootXXXX",
        "Boot 001",
        "Driver0001",
        "Boot",
        "",
        "boot0001",
    ] {
        assert_eq!(bootopt::parse_slot(name), None, "{name} should not parse as a slot");
    }
}

#[test]
fn the_next_free_slot_is_the_lowest_one() {
    assert_eq!(bootopt::next_free_slot(&[]), Some(0));
    assert_eq!(bootopt::next_free_slot(&[0, 1, 2]), Some(3));
    // Gaps are filled, unlike snapshot names. See next_free_slot's docs.
    assert_eq!(bootopt::next_free_slot(&[0, 2, 3]), Some(1));
}

#[test]
fn an_entry_the_firmware_will_not_boot_is_not_styled_like_one_it_will() {
    let active = LoadOption { attributes: LOAD_OPTION_ACTIVE, ..inactive_with(LOAD_OPTION_ACTIVE) };
    assert_eq!(active.style(), Style::Normal);
    assert_eq!(inactive_with(0).style(), Style::Dim);
}

fn inactive_with(attributes: u32) -> LoadOption {
    LoadOption {
        attributes,
        description: "Old Fedora".into(),
        device_path: device_path(),
        optional_data: Vec::new(),
    }
}

#[test]
fn flags_worth_reading_are_named_in_the_detail() {
    let text = gptcore::style::plain(&bootopt::render_flags(&inactive_with(0)));
    assert!(text.contains("inactive"), "{text}");

    let hidden = inactive_with(LOAD_OPTION_ACTIVE | LOAD_OPTION_HIDDEN | LOAD_OPTION_CATEGORY_APP);
    let text = gptcore::style::plain(&bootopt::render_flags(&hidden));
    assert!(!text.contains("inactive"), "{text}");
    assert!(text.contains("hidden"), "{text}");
    assert!(text.contains("application"), "{text}");
}

/// "inactive" is the answer to "why will it not boot?". An ordinary,
/// healthy entry must not produce a line at all, so that the ones that do
/// are the ones worth looking at.
#[test]
fn an_unremarkable_entry_produces_no_detail_lines() {
    let lines = bootopt::render_flags(&inactive_with(LOAD_OPTION_ACTIVE));
    assert!(lines.is_empty(), "{:?}", gptcore::style::plain(&lines));
}

#[test]
fn load_options_are_reported_only_when_there_are_some() {
    let mut opt = inactive_with(LOAD_OPTION_ACTIVE);
    assert!(bootopt::render_flags(&opt).is_empty());
    opt.optional_data = b"root=/dev/sda2".to_vec();
    let text = gptcore::style::plain(&bootopt::render_flags(&opt));
    assert!(text.contains("14 bytes"), "{text}");
}

#[test]
fn the_summary_marks_active_entries_the_way_efibootmgr_does() {
    assert_eq!(bootopt::summary(3, &inactive_with(LOAD_OPTION_ACTIVE)), "Boot0003* Old Fedora");
    assert_eq!(bootopt::summary(3, &inactive_with(0)), "Boot0003  Old Fedora");
}
