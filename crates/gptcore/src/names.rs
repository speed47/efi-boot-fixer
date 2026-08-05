//! The `prefix-NNN.suffix` naming scheme shared by everything this tool
//! saves: `gpt-NNN.bkp`, `boot-NNN.bkp`, `diag-NNN.txt`.
//!
//! One implementation rather than three copies, because the rules are the
//! point and they must not drift apart: names are matched
//! case-insensitively (FAT may hand back `GPT-001.BKP`), and the next
//! number counts up from the highest present and never fills a gap —
//! reusing the number of a file someone deleted would make the ordering
//! lie about which is newest.

use alloc::format;
use alloc::string::String;

/// The number in `prefix-NNN.suffix`, or `None` if `name` is not one.
pub(crate) fn sequence_of(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    if name.len() != prefix.len() + 3 + suffix.len() {
        return None;
    }
    let (head, rest) = name.split_at(prefix.len());
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let (digits, tail) = rest.split_at(3);
    if !tail.eq_ignore_ascii_case(suffix) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The next name to write, given every name already in use, or `None` once
/// the space is exhausted — better to say so than to overwrite somebody's
/// oldest file.
pub(crate) fn next_name(
    existing: &[String],
    prefix: &str,
    suffix: &str,
    max: u32,
) -> Option<String> {
    let highest = existing.iter().filter_map(|n| sequence_of(n, prefix, suffix)).max().unwrap_or(0);
    let next = highest + 1;
    (next <= max).then(|| format!("{prefix}{next:03}{suffix}"))
}
