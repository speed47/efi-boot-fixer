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
///
/// Compared as bytes throughout. Every name this matches is ASCII, but
/// the names it is *asked* about are whatever a FAT directory hands back,
/// and a length in bytes followed by a split at a byte index is a panic
/// waiting for a filename like `写真1.bkp`: the index lands inside a
/// character. This is a filter — it has to be able to say no to anything,
/// including a name that is none of its business.
pub(crate) fn sequence_of(name: &str, prefix: &str, suffix: &str) -> Option<u32> {
    let name = name.as_bytes();
    if name.len() != prefix.len() + 3 + suffix.len() {
        return None;
    }
    let (head, rest) = name.split_at(prefix.len());
    if !head.eq_ignore_ascii_case(prefix.as_bytes()) {
        return None;
    }
    let (digits, tail) = rest.split_at(3);
    if !tail.eq_ignore_ascii_case(suffix.as_bytes()) || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(digits.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0')))
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

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::vec;

    #[test]
    fn a_name_in_the_scheme_yields_its_number() {
        assert_eq!(sequence_of("gpt-001.bkp", "gpt-", ".bkp"), Some(1));
        assert_eq!(sequence_of("gpt-999.bkp", "gpt-", ".bkp"), Some(999));
        assert_eq!(sequence_of("GPT-042.BKP", "gpt-", ".bkp"), Some(42));
        assert_eq!(sequence_of("diag-007.txt", "diag-", ".txt"), Some(7));
    }

    #[test]
    fn anything_else_is_simply_not_one() {
        for name in ["gpt-1.bkp", "gpt-0001.bkp", "gpt-abc.bkp", "gpt-001.txt", "gpt-001", ""] {
            assert_eq!(sequence_of(name, "gpt-", ".bkp"), None, "{name}");
        }
    }

    /// A directory listing is not a list of names this tool wrote. FAT
    /// long names carry any of these, and a byte length that matches while
    /// the split index lands inside a character is what used to abort the
    /// tool the moment a restore screen was opened — or a repair tried to
    /// save its snapshot.
    #[test]
    fn a_name_that_is_not_ascii_is_refused_rather_than_panicked_on() {
        for name in ["gpté11.bkp", "写真1.bkp", "gp写11.bkp"] {
            assert_eq!(sequence_of(name, "gpt-", ".bkp"), None, "{name}");
        }
        for name in ["booté11.bkp", "boot写1.bkp"] {
            assert_eq!(sequence_of(name, "boot-", ".bkp"), None, "{name}");
        }
        assert_eq!(sequence_of("Größe1.txt", "diag-", ".txt"), None);
    }

    #[test]
    fn a_name_nobody_can_parse_does_not_stop_the_numbering() {
        let taken = vec![String::from("gpté11.bkp"), String::from("gpt-004.bkp")];
        assert_eq!(next_name(&taken, "gpt-", ".bkp", 999).as_deref(), Some("gpt-005.bkp"));
    }
}
