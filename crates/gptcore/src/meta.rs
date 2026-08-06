//! The key/value metadata section shared by the snapshot formats.
//!
//! Tab-separated, newline-terminated: trivially readable in a hex dump,
//! which is the situation this data exists for. Deliberately text rather
//! than struct fields — it is read by a person years later trying to work
//! out whether a file belongs to the machine in front of them, and an
//! unknown key they can still read beats a decoder that rejects the file.

use alloc::string::String;
use alloc::vec::Vec;

/// How many pairs a metadata section may carry. Provenance is eight keys
/// or so; this is room to grow and a bound on a file nobody signed.
const MAX_PAIRS: usize = 64;

pub(crate) fn encode_meta(meta: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in meta {
        // A key or value that would break the framing is dropped rather
        // than escaped; nothing this tool writes contains either.
        if k.contains('\t') || k.contains('\n') || v.contains('\n') {
            continue;
        }
        out.extend_from_slice(k.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(v.as_bytes());
        out.push(b'\n');
    }
    out
}

pub(crate) fn decode_meta(bytes: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(text) = core::str::from_utf8(bytes) else {
        return out;
    };
    // The tool writes eight keys or so, and a reader shows one line each.
    // A section this small in every file the tool produces is a section
    // worth bounding, because nothing else does: a megabyte of two-byte
    // pairs expands to tens of megabytes of `String` and as many lines to
    // page through.
    for l in text.lines().take(MAX_PAIRS) {
        if let Some((k, v)) = l.split_once('\t') {
            out.push((String::from(k), String::from(v)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::string::ToString;

    #[test]
    fn a_pair_round_trips() {
        let meta = alloc::vec![("tool".to_string(), "bootfixr 1.1.0".to_string())];
        assert_eq!(decode_meta(&encode_meta(&meta)), meta);
    }

    /// Nothing signs a snapshot, and a reader shows one line per pair.
    #[test]
    fn a_metadata_section_cannot_be_arbitrarily_long() {
        let text = (0..10_000).fold(String::new(), |mut acc, i| {
            use core::fmt::Write;
            let _ = writeln!(acc, "k{i}\tv");
            acc
        });
        assert_eq!(decode_meta(text.as_bytes()).len(), MAX_PAIRS);
    }

    #[test]
    fn text_that_is_not_utf8_is_no_metadata_at_all() {
        assert!(decode_meta(&[0xff, 0xfe, b'\t', b'x']).is_empty());
    }
}
