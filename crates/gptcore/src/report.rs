//! Rendering the operator-facing report.
//!
//! Deliberately free of any UEFI dependency and returning lines rather than
//! printing them, for two reasons: the exact text a person reads before
//! authorising a write deserves tests, and the report can then be rendered
//! on a host for any disk image without booting firmware.
//!
//! Each line carries a [`Style`] saying what it means — damage, caption,
//! a value that must be read — decided here, where the meaning is known.
//! The UEFI application prints the text verbatim and picks colours from
//! the style.

use crate::entry::PartitionEntry;
use crate::guid::Guid;
use crate::layout::{self, Confidence};
use crate::mbr::MbrStatus;
use crate::repair::{Analysis, Implausible, RepairPlan, Step, TableView, Verdict};
use crate::style::{bad, dim, good, key, line, title, warn, Line, Style};
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
        Verdict::MainRepairable => "main GPT is repairable from the secondary GPT",
        Verdict::SecondaryDegraded => {
            "main GPT is fine; the SECONDARY GPT is damaged (not repaired here)"
        }
        Verdict::Unrecoverable => "both tables are damaged - a rescue USB is required",
        Verdict::RefusedHybridMbr => "hybrid MBR present - refusing to touch this disk",
        Verdict::RefusedImplausibleSecondary => {
            "secondary GPT failed sanity checks - refusing to write"
        }
    }
}

/// How serious a verdict is.
///
/// `MainRepairable` is `Warn` rather than `Bad` on purpose: the disk is
/// damaged, but this is the case the tool exists to fix and there is a
/// known-good source for the repair. Reserving `Bad` for the states with no
/// way out keeps it meaningful.
pub fn verdict_style(verdict: Verdict) -> Style {
    match verdict {
        Verdict::Healthy => Style::Good,
        Verdict::MbrOnly | Verdict::MainRepairable | Verdict::SecondaryDegraded => Style::Warn,
        Verdict::Unrecoverable
        | Verdict::RefusedHybridMbr
        | Verdict::RefusedImplausibleSecondary => Style::Bad,
    }
}

fn describe_mbr(status: MbrStatus) -> (String, Style) {
    match status {
        MbrStatus::Protective => ("OK".to_string(), Style::Good),
        MbrStatus::WrongSize { found, expected } => {
            (format!("wrong size ({found} blocks, expected {expected})"), Style::Warn)
        }
        MbrStatus::WrongStart { found } => {
            (format!("wrong start (LBA {found}, expected 1)"), Style::Warn)
        }
        MbrStatus::Hybrid => ("HYBRID - will not touch this disk".to_string(), Style::Bad),
        MbrStatus::Absent => ("missing or not protective".to_string(), Style::Warn),
    }
}

fn push_defects(out: &mut Vec<Line>, label: &str, view: &Result<TableView, IoError>) {
    match view {
        Err(e) => out.push(bad(format!("  {label:<14}: UNREADABLE ({e})"))),
        Ok(t) => {
            if t.is_valid() {
                out.push(good(format!("  {label:<14}: OK")));
                return;
            }
            out.push(bad(format!("  {label:<14}: CORRUPT")));
            for d in &t.defects {
                out.push(bad(format!("      - {d}")));
            }
            if let Some(e) = t.entries_error {
                out.push(bad(format!("      - entry array unreadable ({e})")));
            }
        }
    }
}

/// The per-disk body: geometry, health of each structure, and the verdict.
/// The caller prints the identifying "Disk N: <device path>" line itself.
pub fn render_analysis(analysis: &Analysis) -> Vec<Line> {
    let mut out = Vec::new();
    let capacity = (analysis.last_block + 1).saturating_mul(analysis.block_size as u64);
    // Not dim: on a screen listing several drives this is how you tell
    // which one you are looking at.
    out.push(line(format!(
        "  {} blocks x {} B = {}",
        analysis.last_block + 1,
        analysis.block_size,
        human_size(capacity)
    )));

    let (mbr_text, mbr_style) = describe_mbr(analysis.mbr);
    out.push(Line::new(format!("  {:<14}: {}", "Protective MBR", mbr_text), mbr_style));
    push_defects(&mut out, "Main GPT", &analysis.main);
    push_defects(&mut out, "Secondary GPT", &analysis.secondary);

    if let Some(rec) = &analysis.recognition {
        let (verdict, style) = match rec.confidence {
            Confidence::SteamOs => ("looks like SteamOS", Style::Good),
            Confidence::Partial => ("PARTIAL match to SteamOS", Style::Warn),
            Confidence::Unrecognized => ("NOT recognised as SteamOS", Style::Bad),
        };
        out.push(Line::new(
            format!(
                "  {:<14}: {} ({}/{} expected partitions)",
                "Layout",
                verdict,
                rec.matched.len(),
                layout::STEAMOS_PARTITIONS.len()
            ),
            style,
        ));
        if !rec.missing.is_empty() {
            out.push(warn(format!("      missing: {}", rec.missing.join(", "))));
        }
        for (name, expected, found) in &rec.type_mismatches {
            out.push(warn(format!("      {name} has type {found}, expected {expected}")));
        }
        for (i, guid, name) in &rec.unknown_types {
            out.push(warn(format!("      partition {} has unknown type {guid} ({name})", i + 1)));
        }
    }

    if let Some(reason) = &analysis.rejection {
        out.push(bad(format!("  {:<14}:", "Refused")));
        match reason {
            Implausible::Structure(issues) => {
                for i in issues {
                    out.push(bad(format!("      - {i}")));
                }
            }
            Implausible::Unrecognized(rec) => {
                out.push(bad(format!(
                    "      - no {} in the recovered table",
                    rec.missing_critical.join(" or ")
                )));
            }
            Implausible::EntryArrayCollides { entry_blocks, first_usable } => {
                out.push(bad(format!(
                    "      - entry array of {entry_blocks} blocks would run past first usable LBA {first_usable}"
                )));
            }
        }
    }

    out.push(Line::new(
        format!("  => {}", verdict_line(analysis.verdict)),
        verdict_style(analysis.verdict),
    ));
    out
}

/// The table the operator is being asked to approve.
pub(crate) fn render_table(plan: &RepairPlan, block_size: u32) -> Vec<Line> {
    render_entries(&plan.entries, block_size, plan.header.disk_guid, "Proposed table:")
}

/// A partition list under a caption. Shared by the read-only check, which
/// shows what is on the disk now, and the write paths, which show what
/// would be there afterwards — deliberately the same rendering, so the two
/// can be compared line for line.
pub fn render_entries(
    entries: &[PartitionEntry],
    block_size: u32,
    disk_guid: Guid,
    caption: &str,
) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(Line::blank());
    out.push(title(format!("  {caption}")));
    out.push(dim(format!(
        "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
        "#", "Start LBA", "End LBA", "Size", "Type", "Name"
    )));
    for (i, e) in entries.iter().enumerate().filter(|(_, e)| e.is_used()) {
        let size = e
            .block_count()
            .map(|b| human_size(b.saturating_mul(block_size as u64)))
            .unwrap_or_else(|| "invalid".to_string());
        out.push(line(format!(
            "    {:>2}  {:>12} {:>12} {:>10}  {:<22} {}",
            i + 1,
            e.starting_lba,
            e.ending_lba,
            size,
            layout::describe_type(&e.type_guid),
            e.name_string()
        )));
    }
    out.push(dim(format!("  Disk GUID: {disk_guid}")));
    out
}

/// Exactly what will be written, in order.
pub fn render_plan(plan: &RepairPlan) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(Line::blank());
    out.push(title("  Will write, in this order:"));
    for step in &plan.steps {
        match step {
            // The LBAs are the whole point of showing this screen, so they
            // get the colour that says "read this".
            Step::Write { lba, data, what } => {
                out.push(key(format!("    LBA {:<6} {} ({} bytes)", lba, what, data.len())))
            }
            Step::Flush { why } => out.push(dim(format!("    flush     [{why}]"))),
        }
    }
    out
}

/// Everything for one disk, as the operator sees it.
pub fn render(analysis: &Analysis, plan: Option<&RepairPlan>) -> Vec<Line> {
    let mut out = render_analysis(analysis);
    if let Some(plan) = plan {
        if analysis.verdict == Verdict::MainRepairable {
            out.extend(render_table(plan, analysis.block_size));
        }
        out.extend(render_plan(plan));
    }
    out
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
            Verdict::MainRepairable,
            Verdict::SecondaryDegraded,
            Verdict::Unrecoverable,
            Verdict::RefusedHybridMbr,
            Verdict::RefusedImplausibleSecondary,
        ] {
            assert!(!verdict_line(v).is_empty());
        }
    }

    #[test]
    fn only_hopeless_verdicts_are_styled_as_damage() {
        assert_eq!(verdict_style(Verdict::Healthy), Style::Good);
        // The case the tool exists for: damaged, but fixable from a
        // known-good source.
        assert_eq!(verdict_style(Verdict::MainRepairable), Style::Warn);
        assert_eq!(verdict_style(Verdict::Unrecoverable), Style::Bad);
        assert_eq!(verdict_style(Verdict::RefusedHybridMbr), Style::Bad);
    }
}
