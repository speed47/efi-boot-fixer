//! Console output and the confirmation gate.
//!
//! Sizes are formatted with integer arithmetic only: some UEFI targets
//! build with soft-float and formatting an `f64` is not worth the risk in
//! firmware.

use alloc::format;
use alloc::string::{String, ToString};

use gptcore::layout::{self, Confidence};
use gptcore::mbr::MbrStatus;
use gptcore::repair::{Analysis, Implausible, RepairPlan, Step, TableView, Verdict};
use uefi::boot;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::{print, println, system};

pub fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] =
        [("TiB", 1 << 40), ("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (name, unit) in UNITS {
        if bytes >= unit {
            let whole = bytes / unit;
            let tenths = (bytes % unit) * 10 / unit;
            return format!("{whole}.{tenths} {name}");
        }
    }
    format!("{bytes} B")
}

pub fn banner() {
    println!(
        "efigptfix {} - repair a corrupt primary GPT from the backup",
        env!("CARGO_PKG_VERSION")
    );
    println!();
}

fn status_word(ok: bool) -> &'static str {
    if ok {
        "OK"
    } else {
        "CORRUPT"
    }
}

fn describe_mbr(status: MbrStatus) -> String {
    match status {
        MbrStatus::Protective => "OK".to_string(),
        MbrStatus::WrongSize { found, expected } => {
            format!("wrong size ({found} blocks, expected {expected})")
        }
        MbrStatus::Hybrid => "HYBRID - will not touch this disk".to_string(),
        MbrStatus::Absent => "missing or not protective".to_string(),
    }
}

fn print_defects(label: &str, view: &Result<TableView, gptcore::IoError>) {
    match view {
        Err(e) => println!("  {label:<14}: UNREADABLE ({e})"),
        Ok(t) => {
            println!("  {label:<14}: {}", status_word(t.is_valid()));
            for d in &t.defects {
                println!("      - {d}");
            }
            if let Some(e) = t.entries_error {
                println!("      - entry array unreadable ({e})");
            }
        }
    }
}

pub fn print_analysis(analysis: &Analysis) {
    let capacity = (analysis.last_block + 1) * analysis.block_size as u64;
    println!(
        "  {} blocks x {} B = {}",
        analysis.last_block + 1,
        analysis.block_size,
        human_size(capacity)
    );
    println!("  {:<14}: {}", "Protective MBR", describe_mbr(analysis.mbr));
    print_defects("Primary GPT", &analysis.primary);
    print_defects("Backup GPT", &analysis.backup);

    if let Some(rec) = &analysis.recognition {
        let verdict = match rec.confidence {
            Confidence::SteamOs => "looks like SteamOS",
            Confidence::Partial => "PARTIAL match to SteamOS",
            Confidence::Unrecognized => "NOT recognised as SteamOS",
        };
        println!(
            "  {:<14}: {} ({}/{} expected partitions)",
            "Layout",
            verdict,
            rec.matched.len(),
            layout::STEAMOS_PARTITIONS.len()
        );
        if !rec.missing.is_empty() {
            println!("      missing: {}", rec.missing.join(", "));
        }
        for (name, expected, found) in &rec.type_mismatches {
            println!("      {name} has type {found}, expected {expected}");
        }
        for (i, guid, name) in &rec.unknown_types {
            println!("      partition {} has unknown type {guid} ({name})", i + 1);
        }
    }

    if let Some(reason) = &analysis.rejection {
        println!("  {:<14}:", "Refused");
        match reason {
            Implausible::Structure(issues) => {
                for i in issues {
                    println!("      - {i}");
                }
            }
            Implausible::Unrecognized(rec) => {
                println!("      - no {} in the recovered table", rec.missing_critical.join(" or "));
            }
            Implausible::EntryArrayCollides { entry_blocks, first_usable } => {
                println!(
                    "      - entry array of {entry_blocks} blocks would run past first usable LBA {first_usable}"
                );
            }
        }
    }
}

pub fn verdict_line(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Healthy => "healthy, nothing to do",
        Verdict::MbrOnly => "only the protective MBR needs rewriting",
        Verdict::PrimaryRepairable => "primary GPT is repairable from the backup",
        Verdict::BackupDegraded => "primary is fine; the BACKUP is damaged (not repaired here)",
        Verdict::Unrecoverable => "both tables are damaged - a rescue USB is required",
        Verdict::RefusedHybridMbr => "hybrid MBR present - refusing to touch this disk",
        Verdict::RefusedImplausibleBackup => "backup failed sanity checks - refusing to write",
    }
}

pub fn print_table(plan: &RepairPlan, block_size: u32) {
    println!();
    println!("  Proposed table:");
    println!(
        "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
        "#", "Start LBA", "End LBA", "Size", "Type", "Name"
    );
    for (i, e) in plan.entries.iter().enumerate().filter(|(_, e)| e.is_used()) {
        let size = e
            .block_count()
            .map(|b| human_size(b * block_size as u64))
            .unwrap_or_else(|| "invalid".to_string());
        println!(
            "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
            i + 1,
            e.starting_lba,
            e.ending_lba,
            size,
            layout::describe_type(&e.type_guid),
            e.name_string()
        );
    }
    println!("  Disk GUID: {}", plan.header.disk_guid);
}

pub fn print_plan(plan: &RepairPlan) {
    println!();
    println!("  Will write, in this order:");
    for step in &plan.steps {
        match step {
            Step::Write { lba, data, what } => {
                println!("    LBA {:<6} {} ({} bytes)", lba, what, data.len())
            }
            Step::Flush { why } => println!("    flush     [{why}]"),
        }
    }
}

fn next_key() -> Option<Key> {
    system::with_stdin(|stdin| stdin.read_key().ok().flatten())
}

/// Block until the operator types `word` exactly, or presses Escape.
///
/// A single keypress would be too easy to hit by accident for something
/// that rewrites a partition table.
pub fn confirm(word: &str) -> bool {
    print!("  Type {word} then Enter to write, or Esc to skip: ");
    let mut typed = String::new();
    loop {
        let Some(key) = next_key() else {
            boot::stall(10_000);
            continue;
        };
        match key {
            Key::Special(ScanCode::ESCAPE) => {
                println!();
                return false;
            }
            Key::Printable(c) => match char::from(c) {
                '\r' | '\n' => {
                    println!();
                    return typed == word;
                }
                '\u{8}' => {
                    if typed.pop().is_some() {
                        print!("\u{8} \u{8}");
                    }
                }
                ch => {
                    typed.push(ch);
                    print!("{ch}");
                }
            },
            _ => {}
        }
    }
}

/// Block until Enter or Escape, discarding anything else.
pub fn wait_for_enter() {
    loop {
        let Some(key) = next_key() else {
            boot::stall(10_000);
            continue;
        };
        match key {
            Key::Special(ScanCode::ESCAPE) => return,
            Key::Printable(c) if matches!(char::from(c), '\r' | '\n') => return,
            _ => {}
        }
    }
}
