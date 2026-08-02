//! Saving the whole boot configuration to a file before changing it.
//!
//! `BootOrder` and the `Boot####` entries are small — a few hundred bytes
//! all told — and they are the only copy. There is no backup `BootOrder`
//! at the far end of NVRAM the way there is a backup GPT at the far end of
//! a disk, so an edit made from a screen with no keyboard has nothing
//! behind it unless something puts a copy somewhere first.
//!
//! This is that copy: every global variable the boot process depends on,
//! stored verbatim, next to the GPT snapshots on the ESP. Variables are
//! held as opaque name/bytes pairs and are *not* re-encoded on the way in
//! or out — a `Boot####` this build cannot parse is exactly the one worth
//! having a byte-for-byte copy of.
//!
//! Same shape as [`crate::backup`]: little-endian, a CRC32 over everything
//! before it, and a metadata section recording what was true when it was
//! taken.

use crate::backup::Timestamp;
use crate::crc::Crc32;
use crate::style::{dim, key, title, Line};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub(crate) const MAGIC: [u8; 8] = *b"EFIBOOTC";
pub const VERSION: u32 = 1;

/// Snapshots are `boot.NNN`, alongside the GPT snapshots' `gpt.NNN`.
pub(crate) const NAME_PREFIX: &str = "boot.";
pub const MAX_SEQUENCE: u32 = 999;

/// Magic, version, timestamp, variable count.
const FIXED_LEN: usize = 8 + 4 + 7 + 4;

/// The boot configuration as it stood, ready to encode.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot {
    pub time: Timestamp,
    /// Variable name and its raw bytes, in the order they were read.
    pub vars: Vec<(String, Vec<u8>)>,
    pub meta: Vec<(String, String)>,
}

impl Snapshot {
    /// How many of the variables are boot entries rather than settings.
    pub fn entry_count(&self) -> usize {
        self.vars.iter().filter(|(n, _)| crate::bootopt::parse_slot(n).is_some()).count()
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.vars.iter().find(|(n, _)| n == name).map(|(_, d)| d.as_slice())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    TooShort,
    BadMagic,
    UnsupportedVersion {
        found: u32,
    },
    BadChecksum {
        stored: u32,
        computed: u32,
    },
    /// A length field points past the end of the file.
    Truncated {
        at: usize,
    },
    /// A variable name that is not UTF-8.
    BadName,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "file is too short to be a boot snapshot"),
            DecodeError::BadMagic => write!(f, "not a boot snapshot"),
            DecodeError::UnsupportedVersion { found } => {
                write!(f, "written by a later build (version {found})")
            }
            DecodeError::BadChecksum { stored, computed } => {
                write!(f, "checksum mismatch (stored {stored:#x}, computed {computed:#x})")
            }
            DecodeError::Truncated { at } => write!(f, "file is truncated at byte {at}"),
            DecodeError::BadName => write!(f, "file holds a variable name that is not text"),
        }
    }
}

fn le_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

pub fn encode(snap: &Snapshot, crc: &impl Crc32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&snap.time.year.to_le_bytes());
    out.push(snap.time.month);
    out.push(snap.time.day);
    out.push(snap.time.hour);
    out.push(snap.time.minute);
    out.push(snap.time.second);
    out.extend_from_slice(&(snap.vars.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), FIXED_LEN);

    for (name, data) in &snap.vars {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }

    let meta = encode_meta(&snap.meta);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);

    let sum = crc.crc32(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

fn encode_meta(meta: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in meta {
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

fn decode_meta(bytes: &[u8]) -> Vec<(String, String)> {
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

pub fn decode(bytes: &[u8], crc: &impl Crc32) -> Result<Snapshot, DecodeError> {
    if bytes.len() < FIXED_LEN + 4 {
        return Err(DecodeError::TooShort);
    }
    if bytes[..8] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = le_u32(bytes, 8);
    if version == 0 || version > VERSION {
        return Err(DecodeError::UnsupportedVersion { found: version });
    }

    let body = &bytes[..bytes.len() - 4];
    let stored = le_u32(bytes, bytes.len() - 4);
    let computed = crc.crc32(body);
    if stored != computed {
        return Err(DecodeError::BadChecksum { stored, computed });
    }

    let time = Timestamp {
        year: le_u16(bytes, 12),
        month: bytes[14],
        day: bytes[15],
        hour: bytes[16],
        minute: bytes[17],
        second: bytes[18],
    };

    let count = le_u32(bytes, 19) as usize;
    let mut at = FIXED_LEN;
    let mut vars = Vec::new();
    for _ in 0..count {
        // Each of these four steps is a place a truncated file can claim a
        // length the file does not have.
        if at + 2 > body.len() {
            return Err(DecodeError::Truncated { at });
        }
        let name_len = le_u16(body, at) as usize;
        at += 2;
        if at + name_len > body.len() {
            return Err(DecodeError::Truncated { at });
        }
        let name =
            core::str::from_utf8(&body[at..at + name_len]).map_err(|_| DecodeError::BadName)?;
        at += name_len;

        if at + 4 > body.len() {
            return Err(DecodeError::Truncated { at });
        }
        let data_len = le_u32(body, at) as usize;
        at += 4;
        if at + data_len > body.len() {
            return Err(DecodeError::Truncated { at });
        }
        vars.push((String::from(name), body[at..at + data_len].to_vec()));
        at += data_len;
    }

    if at + 4 > body.len() {
        return Err(DecodeError::Truncated { at });
    }
    let meta_len = le_u32(body, at) as usize;
    at += 4;
    if at + meta_len > body.len() {
        return Err(DecodeError::Truncated { at });
    }
    let meta = decode_meta(&body[at..at + meta_len]);

    Ok(Snapshot { time, vars, meta })
}

/// The number in `boot.NNN`, case-insensitively: FAT may hand back
/// `BOOT.001`.
pub fn sequence_of(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    for expected in NAME_PREFIX.chars() {
        if !chars.next()?.eq_ignore_ascii_case(&expected) {
            return None;
        }
    }
    let digits: &str = &name[NAME_PREFIX.len()..];
    if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The next name to write, given what is already there.
///
/// Counts from the highest rather than filling gaps, for the same reason
/// [`crate::backup::next_name`] does: reusing the number of a snapshot
/// someone deleted would make the ordering lie about which is newest.
/// Note that this is the opposite of [`crate::bootopt::next_free_slot`],
/// deliberately — a filename holds the only copy of something, a boot slot
/// holds nothing.
pub fn next_name(existing: &[String]) -> Option<String> {
    let highest = existing.iter().filter_map(|n| sequence_of(n)).max().unwrap_or(0);
    let next = highest + 1;
    (next <= MAX_SEQUENCE).then(|| format!("{NAME_PREFIX}{next:03}"))
}

/// What was saved, for the screen shown after saving it.
pub fn describe(snap: &Snapshot) -> Vec<Line> {
    let mut out = alloc::vec![key(format!("  Taken:   {}", snap.time))];
    out.push(dim(format!(
        "  Holds:   {} boot entries and {} other variables",
        snap.entry_count(),
        snap.vars.len() - snap.entry_count()
    )));
    out.push(Line::blank());
    out.push(title("  Contains:"));
    for (name, data) in &snap.vars {
        out.push(dim(format!("    {:<12} {} bytes", name, data.len())));
    }
    out
}
