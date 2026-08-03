//! `tools/deck-corrupt.py` against the real Steam Deck fixture.
//!
//! The script is what will be pointed at actual hardware, so the claims it
//! makes are worth checking mechanically: that it reproduces the exact
//! corruption that was found in the wild, that the snapshot it writes is
//! readable by the application that has to restore it when the machine
//! will not boot, and that both recovery routes — restore the snapshot, or
//! repair from the secondary GPT — put the disk back byte for byte.
//!
//! Skipped if python3 is unavailable rather than failing: the script is a
//! testing aid, not part of the product.

mod common;

use common::deck_image;
use gptcore::backup::{decode, restore_plan, Health, Role};
use gptcore::header::Defect;
use gptcore::{analyze, apply, plan, SoftCrc32, Verdict};
use std::path::{Path, PathBuf};
use std::process::Command;

const CRC: SoftCrc32 = SoftCrc32;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/deck-corrupt.py")
}

fn python() -> Option<&'static str> {
    Command::new("python3").arg("--version").output().ok().map(|_| "python3")
}

fn run(args: &[&str]) -> String {
    let out = Command::new("python3")
        .arg(script())
        .args(args)
        .output()
        .expect("failed to run deck-corrupt.py");
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "deck-corrupt.py {args:?} failed:\n{text}");
    text
}

fn snapshot_path(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("gptsnap-{tag}-{n}.bin"))
}

#[test]
fn it_reproduces_the_corruption_that_was_found_in_the_wild() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("break");
    let before = img.read_lba(0, 34);

    let out = run(&["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"]);
    assert!(out.contains("PartitionEntryLBA  2 -> 2016"), "{out}");
    assert!(out.contains("About to change 6 bytes"), "{out}");

    // Exactly the two fields, and nothing else in the first 34 sectors.
    let after = img.read_lba(0, 34);
    let changed: Vec<usize> = (0..before.len()).filter(|i| before[*i] != after[*i]).collect();
    assert_eq!(changed, vec![512 + 16, 512 + 17, 512 + 18, 512 + 19, 512 + 72, 512 + 73]);

    // And gptcore agrees this is the failure it was written for.
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    assert_eq!(analysis.verdict, Verdict::MainRepairable);
    let main = analysis.main.as_ref().expect("main GPT readable");
    assert!(
        main.defects.contains(&Defect::MainEntryLbaNotTwo { found: 2016 }),
        "{:?}",
        main.defects
    );
    // The header CRC still verifies, which is what makes this nasty.
    assert!(
        !main.defects.iter().any(|d| matches!(d, Defect::HeaderCrcMismatch { .. })),
        "{:?}",
        main.defects
    );

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn the_snapshot_it_writes_is_readable_by_the_application() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("save");
    run(&["save", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap()]);

    let bytes = std::fs::read(&snap).expect("snapshot");
    let archive = decode(&bytes, &CRC).expect("the EFI application must be able to decode this");

    assert_eq!(archive.block_size, 512);
    assert_eq!(archive.last_block, img.last_block());
    assert_eq!(archive.health, Health::Healthy);
    assert_eq!(archive.chunks.len(), 5);
    assert_eq!(archive.chunk(Role::MainEntries).unwrap().lba, 2);
    assert_eq!(archive.chunk(Role::SecondaryHeader).unwrap().lba, img.last_block());
    assert_eq!(archive.chunk(Role::MainHeader).unwrap().data, img.read_lba(1, 1));

    // The script records its own provenance in the same key/value section
    // the application reads, so a snapshot taken from Linux is as
    // self-describing as one taken from firmware.
    assert_eq!(archive.version, gptcore::backup::VERSION);
    assert!(
        archive.meta_get("tool").is_some_and(|t| t.starts_with("deck-corrupt.py")),
        "{:?}",
        archive.meta
    );
    assert!(archive.meta_get("device").is_some());
    assert!(archive.meta_get("host").is_some());

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn restoring_the_scripts_snapshot_undoes_the_break() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("undo");
    let before = img.read_lba(0, 34);

    run(&["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"]);
    assert_ne!(img.read_lba(0, 34), before);

    // Through the application's own code path, not the script's.
    let archive = decode(&std::fs::read(&snap).unwrap(), &CRC).expect("decode");
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let restore = restore_plan(&archive, &analysis).expect("plan");
    apply(&mut disk, &restore).expect("apply");

    assert_eq!(img.read_lba(0, 34), before, "restore was not byte-exact");
    assert!(img.is_clean(), "{}", img.verify());

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn repairing_from_the_secondary_gpt_undoes_the_break_too() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("repair");
    let before = img.read_lba(0, 34);

    run(&["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"]);

    // No snapshot involved: this is the path that has to work when the
    // only thing left is the secondary GPT at the end of the disk.
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    let repair = plan(&analysis, &CRC).expect("a repair plan");
    apply(&mut disk, &repair).expect("apply");

    assert_eq!(img.read_lba(0, 34), before, "repair did not restore the original bytes");
    assert!(img.is_clean(), "{}", img.verify());

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn the_scripts_own_restore_works_without_the_application() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("selfundo");
    let before = img.read_lba(0, 34);

    run(&["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"]);
    run(&["restore", img.path.to_str().unwrap(), "-i", snap.to_str().unwrap()]);

    assert_eq!(img.read_lba(0, 34), before);
    assert!(img.is_clean(), "{}", img.verify());

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn it_refuses_a_disk_it_has_already_broken() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("twice");
    run(&["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"]);

    let out = Command::new("python3")
        .arg(script())
        .args(["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"])
        .output()
        .expect("run");
    assert!(!out.status.success(), "a second break should have been refused");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("already not healthy") || text.contains("already 2016"), "{text}");

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn a_damaged_snapshot_is_refused_by_the_scripts_restore() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("damaged");
    run(&["save", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap()]);

    let mut bytes = std::fs::read(&snap).unwrap();
    let n = bytes.len() / 2;
    bytes[n] ^= 0x40;
    std::fs::write(&snap, &bytes).unwrap();

    let out = Command::new("python3")
        .arg(script())
        .args(["restore", img.path.to_str().unwrap(), "-i", snap.to_str().unwrap()])
        .output()
        .expect("run");
    assert!(!out.status.success(), "a corrupted snapshot must not be restored");
    assert!(String::from_utf8_lossy(&out.stderr).contains("checksum"));

    let _ = std::fs::remove_file(&snap);
}

#[test]
fn it_refuses_when_the_secondary_gpt_could_not_repair_the_damage() {
    if python().is_none() {
        return;
    }
    let img = deck_image();
    let snap = snapshot_path("nobackup");

    // Destroy the secondary GPT header. Breaking the main GPT now would leave
    // nothing to recover from, which is the one outcome this script must
    // never produce.
    img.zero_lba(img.last_block(), 1);

    let out = Command::new("python3")
        .arg(script())
        .args(["break", img.path.to_str().unwrap(), "-o", snap.to_str().unwrap(), "--yes"])
        .output()
        .expect("run");
    assert!(!out.status.success(), "breaking a disk with no usable secondary GPT must be refused");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("SECONDARY GPT does not verify"), "{text}");

    // And it really did not touch the main GPT, nor leave a snapshot behind.
    let mut disk = img.disk();
    let analysis = analyze(&mut disk, &CRC).expect("analyze");
    assert_eq!(analysis.main.as_ref().unwrap().header.partition_entry_lba, 2);
    assert!(!snap.exists(), "a refused break must not leave a snapshot");
}
