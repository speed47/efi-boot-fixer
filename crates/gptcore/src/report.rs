//! Rendering the operator-facing report.
//!
//! Deliberately free of any UEFI dependency and returning lines rather than
//! printing them, for two reasons: the exact text a person reads before
//! authorising a write deserves tests, and the report can then be rendered
//! on a host for any disk image without booting firmware.
//!
//! The UEFI application prints these lines verbatim.

use crate::header::Defect;
use crate::layout::{self, Confidence};
use crate::mbr::MbrStatus;
use crate::repair::{Analysis, Implausible, RepairPlan, Step, TableView, Verdict};
use crate::IoError;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Integer-only size formatting: some UEFI targets build with soft-float,
/// and formatting an `f64` is not worth the risk in firmware.
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

fn push_defects(out: &mut Vec<String>, label: &str, view: &Result<TableView, IoError>) {
    match view {
        Err(e) => out.push(format!("  {label:<14}: UNREADABLE ({e})")),
        Ok(t) => {
            let state = if t.is_valid() { "OK" } else { "CORRUPT" };
            out.push(format!("  {label:<14}: {state}"));
            for d in &t.defects {
                out.push(format!("      - {d}"));
            }
            if let Some(e) = t.entries_error {
                out.push(format!("      - entry array unreadable ({e})"));
            }
        }
    }
}

/// The per-disk body: geometry, health of each structure, and the verdict.
/// The caller prints the identifying "Disk N: <device path>" line itself.
pub fn render_analysis(analysis: &Analysis) -> Vec<String> {
    let mut out = Vec::new();
    let capacity = (analysis.last_block + 1).saturating_mul(analysis.block_size as u64);
    out.push(format!(
        "  {} blocks x {} B = {}",
        analysis.last_block + 1,
        analysis.block_size,
        human_size(capacity)
    ));
    out.push(format!("  {:<14}: {}", "Protective MBR", describe_mbr(analysis.mbr)));
    push_defects(&mut out, "Primary GPT", &analysis.primary);
    push_defects(&mut out, "Backup GPT", &analysis.backup);

    if let Some(rec) = &analysis.recognition {
        let verdict = match rec.confidence {
            Confidence::SteamOs => "looks like SteamOS",
            Confidence::Partial => "PARTIAL match to SteamOS",
            Confidence::Unrecognized => "NOT recognised as SteamOS",
        };
        out.push(format!(
            "  {:<14}: {} ({}/{} expected partitions)",
            "Layout",
            verdict,
            rec.matched.len(),
            layout::STEAMOS_PARTITIONS.len()
        ));
        if !rec.missing.is_empty() {
            out.push(format!("      missing: {}", rec.missing.join(", ")));
        }
        for (name, expected, found) in &rec.type_mismatches {
            out.push(format!("      {name} has type {found}, expected {expected}"));
        }
        for (i, guid, name) in &rec.unknown_types {
            out.push(format!("      partition {} has unknown type {guid} ({name})", i + 1));
        }
    }

    if let Some(reason) = &analysis.rejection {
        out.push(format!("  {:<14}:", "Refused"));
        match reason {
            Implausible::Structure(issues) => {
                for i in issues {
                    out.push(format!("      - {i}"));
                }
            }
            Implausible::Unrecognized(rec) => {
                out.push(format!(
                    "      - no {} in the recovered table",
                    rec.missing_critical.join(" or ")
                ));
            }
            Implausible::EntryArrayCollides { entry_blocks, first_usable } => {
                out.push(format!(
                    "      - entry array of {entry_blocks} blocks would run past first usable LBA {first_usable}"
                ));
            }
        }
    }

    out.push(format!("  => {}", verdict_line(analysis.verdict)));
    out
}

/// The table the operator is being asked to approve.
pub fn render_table(plan: &RepairPlan, block_size: u32) -> Vec<String> {
    let mut out = Vec::new();
    out.push(String::new());
    out.push("  Proposed table:".to_string());
    out.push(format!(
        "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
        "#", "Start LBA", "End LBA", "Size", "Type", "Name"
    ));
    for (i, e) in plan.entries.iter().enumerate().filter(|(_, e)| e.is_used()) {
        let size = e
            .block_count()
            .map(|b| human_size(b.saturating_mul(block_size as u64)))
            .unwrap_or_else(|| "invalid".to_string());
        out.push(format!(
            "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
            i + 1,
            e.starting_lba,
            e.ending_lba,
            size,
            layout::describe_type(&e.type_guid),
            e.name_string()
        ));
    }
    out.push(format!("  Disk GUID: {}", plan.header.disk_guid));
    out
}

/// Exactly what will be written, in order.
pub fn render_plan(plan: &RepairPlan) -> Vec<String> {
    let mut out = Vec::new();
    out.push(String::new());
    out.push("  Will write, in this order:".to_string());
    for step in &plan.steps {
        match step {
            Step::Write { lba, data, what } => {
                out.push(format!("    LBA {:<6} {} ({} bytes)", lba, what, data.len()))
            }
            Step::Flush { why } => out.push(format!("    flush     [{why}]")),
        }
    }
    out
}

/// Everything for one disk, as the operator sees it.
pub fn render(analysis: &Analysis, plan: Option<&RepairPlan>) -> Vec<String> {
    let mut out = render_analysis(analysis);
    if let Some(plan) = plan {
        if analysis.verdict == Verdict::PrimaryRepairable {
            out.extend(render_table(plan, analysis.block_size));
        }
        out.extend(render_plan(plan));
    }
    out
}

/// Used by the application when a defect list needs printing on its own.
pub fn render_defects(defects: &[Defect]) -> Vec<String> {
    defects.iter().map(|d| format!("      - {d}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    #[test]
    fn sizes_use_integer_arithmetic() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(64 * 1024 * 1024), "64.0 MiB");
        assert_eq!(human_size(1_953_525_168 * 512), "931.5 GiB");
    }

    #[test]
    fn every_verdict_has_a_line() {
        for v in [
            Verdict::Healthy,
            Verdict::MbrOnly,
            Verdict::PrimaryRepairable,
            Verdict::BackupDegraded,
            Verdict::Unrecoverable,
            Verdict::RefusedHybridMbr,
            Verdict::RefusedImplausibleBackup,
        ] {
            assert!(!verdict_line(v).is_empty());
        }
    }
}
