//! The key/value metadata section shared by the snapshot formats.
//!
//! Tab-separated, newline-terminated: trivially readable in a hex dump,
//! which is the situation this data exists for. Deliberately text rather
//! than struct fields — it is read by a person years later trying to work
//! out whether a file belongs to the machine in front of them, and an
//! unknown key they can still read beats a decoder that rejects the file.

use alloc::string::String;
use alloc::vec::Vec;

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
    for l in text.lines() {
        if let Some((k, v)) = l.split_once('\t') {
            out.push((String::from(k), String::from(v)));
        }
    }
    out
}
