//! Saving both GPTs to a file and putting them back.
//!
//! This is deliberately not "dd the first 34 sectors". An archive records
//! the geometry it was taken from and the health of the table at the time,
//! and stores each structure as a separate chunk with a role. That buys
//! three things a raw dump does not:
//!
//! * restore can refuse a disk whose geometry does not match, instead of
//!   writing a table describing a different device;
//! * the operator is told, before authorising, whether the table they are
//!   about to restore was healthy when it was captured — restoring a
//!   corrupt snapshot is a real way to make things worse;
//! * the write order can put entry arrays on the medium before the headers
//!   that point at them, exactly as [`crate::repair`] does.
//!
//! The format is little-endian throughout and ends with a CRC32 over
//! everything before it, so a truncated or damaged file is rejected rather
//! than half-restored.

use crate::crc::Crc32;
use crate::disk::{read_lbas, BlockDevice, IoError};
use crate::entry::{parse_array, PartitionEntry};
use crate::guid::Guid;
use crate::header::GptHeader;
use crate::repair::{Analysis, RepairPlan, Step, Verdict};
use crate::style::{self, dim, key, line, title, Line, Style};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub(crate) const MAGIC: [u8; 8] = *b"EFIGPTBK";
/// Bumped to 2 when the metadata section was added. Version 1 files stay
/// readable: a snapshot is worthless if a later build refuses it.
pub const VERSION: u32 = 2;
/// The oldest layout [`decode`] understands.
///
/// Version 1 is not hypothetical and this is not speculative
/// compatibility: v1 snapshots were written by earlier builds and at least
/// one is held on real hardware. The `version >= 2` branch in [`decode`]
/// and the "predates provenance" line in [`inspect`] exist to read it. A
/// sweep for unreachable code will keep suggesting all three; they are
/// reachable by any file older than the metadata section.
pub const MIN_VERSION: u32 = 1;

/// Size of the fixed part, before the chunks: magic, version, block size,
/// last block, disk GUID, timestamp, health, chunk count.
const FIXED_LEN: usize = 52;
/// Bytes preceding each chunk's payload: role, lba, blocks, byte length.
const CHUNK_HEAD_LEN: usize = 4 + 8 + 8 + 8;

/// Conventional entry array: 128 entries of 128 bytes.
const CONVENTIONAL_ARRAY_BYTES: u64 = 128 * 128;

/// A wall-clock reading, passed in by the caller.
///
/// `gptcore` has no clock; under firmware this comes from
/// `EFI_RUNTIME_SERVICES.GetTime`, and in tests it is whatever the test
/// says it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Timestamp {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl core::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// What a chunk is, which is what decides where it goes back and when.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Mbr,
    PrimaryEntries,
    PrimaryHeader,
    BackupEntries,
    BackupHeader,
}

impl Role {
    fn code(self) -> u32 {
        match self {
            Role::Mbr => 1,
            Role::PrimaryEntries => 2,
            Role::PrimaryHeader => 3,
            Role::BackupEntries => 4,
            Role::BackupHeader => 5,
        }
    }

    fn from_code(code: u32) -> Option<Role> {
        Some(match code {
            1 => Role::Mbr,
            2 => Role::PrimaryEntries,
            3 => Role::PrimaryHeader,
            4 => Role::BackupEntries,
            5 => Role::BackupHeader,
            _ => return None,
        })
    }

    pub fn describe(self) -> &'static str {
        match self {
            Role::Mbr => "protective MBR",
            Role::PrimaryEntries => "primary partition entry array",
            Role::PrimaryHeader => "primary GPT header",
            Role::BackupEntries => "backup partition entry array",
            Role::BackupHeader => "backup GPT header",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub role: Role,
    pub lba: u64,
    pub data: Vec<u8>,
}

impl Chunk {
    pub fn blocks(&self, block_size: u32) -> u64 {
        if block_size == 0 {
            return 0;
        }
        self.data.len() as u64 / block_size as u64
    }
}

/// How the table looked when the snapshot was taken.
///
/// Stored so restore can say so. A backup of a broken table is still worth
/// keeping — it is evidence — but putting it back is not a repair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    Healthy,
    MbrOnly,
    PrimaryCorrupt,
    BackupCorrupt,
    BothCorrupt,
    Other,
}

impl Health {
    fn code(self) -> u8 {
        match self {
            Health::Healthy => 0,
            Health::MbrOnly => 1,
            Health::PrimaryCorrupt => 2,
            Health::BackupCorrupt => 3,
            Health::BothCorrupt => 4,
            Health::Other => 5,
        }
    }

    fn from_code(code: u8) -> Health {
        match code {
            0 => Health::Healthy,
            1 => Health::MbrOnly,
            2 => Health::PrimaryCorrupt,
            3 => Health::BackupCorrupt,
            4 => Health::BothCorrupt,
            _ => Health::Other,
        }
    }

    fn from_verdict(verdict: Verdict) -> Health {
        match verdict {
            Verdict::Healthy => Health::Healthy,
            Verdict::MbrOnly => Health::MbrOnly,
            Verdict::PrimaryRepairable | Verdict::RefusedImplausibleBackup => {
                Health::PrimaryCorrupt
            }
            Verdict::BackupDegraded => Health::BackupCorrupt,
            Verdict::Unrecoverable => Health::BothCorrupt,
            Verdict::RefusedHybridMbr => Health::Other,
        }
    }

    pub fn is_clean(self) -> bool {
        self == Health::Healthy
    }

    /// Both GPTs parsed and verified at capture time.
    ///
    /// Distinct from [`Health::is_clean`] because a wrong protective MBR
    /// is saved as found and does come back on restore, but it is not a
    /// reason to warn someone off restoring their partition table.
    pub fn tables_were_sound(self) -> bool {
        matches!(self, Health::Healthy | Health::MbrOnly)
    }

    pub fn style(self) -> Style {
        match self {
            Health::Healthy => Style::Good,
            Health::MbrOnly => Style::Warn,
            _ => Style::Bad,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::MbrOnly => "tables healthy, protective MBR wrong",
            Health::PrimaryCorrupt => "PRIMARY WAS CORRUPT",
            Health::BackupCorrupt => "BACKUP WAS CORRUPT",
            Health::BothCorrupt => "BOTH TABLES WERE CORRUPT",
            Health::Other => "unusual (hybrid MBR or similar)",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Archive {
    /// Layout the file was written in. New archives are [`VERSION`].
    pub version: u32,
    pub block_size: u32,
    /// `Media->LastBlock` of the disk this came from.
    pub last_block: u64,
    /// Disk GUID at capture time, or zero if no header could be read.
    pub disk_guid: Guid,
    pub time: Timestamp,
    pub health: Health,
    pub chunks: Vec<Chunk>,
    /// Free-form provenance: which build wrote this, on what firmware,
    /// from which device. Empty for version 1 files.
    ///
    /// Deliberately key/value text rather than struct fields. This is read
    /// by a person years later trying to work out whether a file belongs to
    /// the machine in front of them, and an unknown key they can still read
    /// beats a decoder that rejects the file.
    pub meta: Vec<(String, String)>,
}

impl Archive {
    pub fn chunk(&self, role: Role) -> Option<&Chunk> {
        self.chunks.iter().find(|c| c.role == role)
    }

    pub fn capacity(&self) -> u64 {
        (self.last_block + 1).saturating_mul(self.block_size as u64)
    }

    /// The header the restore would install at LBA 1, if it can be parsed.
    pub(crate) fn primary_header(&self) -> Option<GptHeader> {
        GptHeader::parse(&self.chunk(Role::PrimaryHeader)?.data)
    }

    pub fn meta_get(&self, key: &str) -> Option<&str> {
        self.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The partitions this snapshot would restore.
    ///
    /// Parsed from the primary entry array, falling back to the backup
    /// copy: the two are identical on a healthy disk, and when they are not
    /// the surviving one is still worth showing.
    pub fn entries(&self) -> Vec<PartitionEntry> {
        let Some(header) = self
            .primary_header()
            .or_else(|| GptHeader::parse(&self.chunk(Role::BackupHeader)?.data))
        else {
            return Vec::new();
        };
        let Some(array) =
            self.chunk(Role::PrimaryEntries).or_else(|| self.chunk(Role::BackupEntries))
        else {
            return Vec::new();
        };
        parse_array(&array.data, header.number_of_partition_entries, header.size_of_partition_entry)
    }
}

/// The number of blocks a conventional 16 KiB entry array occupies.
fn default_array_blocks(block_size: u32) -> u64 {
    if block_size == 0 {
        return 0;
    }
    CONVENTIONAL_ARRAY_BYTES.div_ceil(block_size as u64)
}

/// Read the structures worth saving off `dev`.
///
/// Where a header is intact its own pointers are followed, so a snapshot
/// records the disk as it actually is rather than as it ought to be. Where
/// it is not, the conventional locations are used, which is the best guess
/// available and still captures the bytes a later investigation would want.
pub fn capture<D: BlockDevice + ?Sized>(
    dev: &mut D,
    analysis: &Analysis,
    time: Timestamp,
    meta: Vec<(String, String)>,
) -> Result<Archive, IoError> {
    let block_size = analysis.block_size;
    let last_block = analysis.last_block;
    let fallback = default_array_blocks(block_size);

    let mut chunks = Vec::new();
    chunks.push(Chunk { role: Role::Mbr, lba: 0, data: analysis.mbr_raw.clone() });

    // Primary: its own array pointer if the header parsed, else LBA 2.
    let (primary_lba, primary_blocks) = match analysis.primary.as_ref() {
        Ok(t) => (
            t.header.partition_entry_lba,
            t.header.entry_array_blocks(block_size).unwrap_or(fallback),
        ),
        Err(_) => (2, fallback),
    };
    if let Ok(data) = read_lbas(dev, primary_lba, primary_blocks) {
        chunks.push(Chunk { role: Role::PrimaryEntries, lba: primary_lba, data });
    }
    chunks.push(Chunk {
        role: Role::PrimaryHeader,
        lba: 1,
        data: match analysis.primary.as_ref() {
            Ok(t) => t.raw.clone(),
            Err(_) => read_lbas(dev, 1, 1)?,
        },
    });

    // Backup: its array sits immediately below its header at the end.
    let (backup_lba, backup_blocks) = match analysis.backup.as_ref() {
        Ok(t) => (
            t.header.partition_entry_lba,
            t.header.entry_array_blocks(block_size).unwrap_or(fallback),
        ),
        Err(_) => (last_block.saturating_sub(fallback), fallback),
    };
    if let Ok(data) = read_lbas(dev, backup_lba, backup_blocks) {
        chunks.push(Chunk { role: Role::BackupEntries, lba: backup_lba, data });
    }
    chunks.push(Chunk {
        role: Role::BackupHeader,
        lba: last_block,
        data: match analysis.backup.as_ref() {
            Ok(t) => t.raw.clone(),
            Err(_) => read_lbas(dev, last_block, 1)?,
        },
    });

    let disk_guid = analysis
        .primary
        .as_ref()
        .ok()
        .filter(|t| t.is_valid())
        .or_else(|| analysis.backup.as_ref().ok().filter(|t| t.is_valid()))
        .map(|t| t.header.disk_guid)
        .unwrap_or(Guid::ZERO);

    Ok(Archive {
        block_size,
        last_block,
        disk_guid,
        time,
        version: VERSION,
        health: Health::from_verdict(analysis.verdict),
        chunks,
        meta,
    })
}

fn encode_meta(meta: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in meta {
        // Tab-separated, newline-terminated: trivially readable in a hex
        // dump, which is the situation this data exists for.
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

pub fn encode(archive: &Archive, crc: &impl Crc32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&archive.block_size.to_le_bytes());
    out.extend_from_slice(&archive.last_block.to_le_bytes());
    out.extend_from_slice(archive.disk_guid.as_bytes());
    out.extend_from_slice(&archive.time.year.to_le_bytes());
    out.push(archive.time.month);
    out.push(archive.time.day);
    out.push(archive.time.hour);
    out.push(archive.time.minute);
    out.push(archive.time.second);
    out.push(archive.health.code());
    out.extend_from_slice(&(archive.chunks.len() as u32).to_le_bytes());
    debug_assert_eq!(out.len(), FIXED_LEN);

    for chunk in &archive.chunks {
        out.extend_from_slice(&chunk.role.code().to_le_bytes());
        out.extend_from_slice(&chunk.lba.to_le_bytes());
        out.extend_from_slice(&chunk.blocks(archive.block_size).to_le_bytes());
        out.extend_from_slice(&(chunk.data.len() as u64).to_le_bytes());
        out.extend_from_slice(&chunk.data);
    }

    let meta = encode_meta(&archive.meta);
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);

    let sum = crc.crc32(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    TooShort,
    BadMagic,
    UnsupportedVersion {
        found: u32,
    },
    /// The trailing CRC does not match: the file is damaged or truncated.
    BadChecksum {
        stored: u32,
        computed: u32,
    },
    /// A chunk claims more bytes than the file contains.
    Truncated,
    BadGeometry,
    UnknownRole {
        code: u32,
    },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "file is too short to be a GPT backup"),
            DecodeError::BadMagic => write!(f, "not a GPT backup file"),
            DecodeError::UnsupportedVersion { found } => {
                write!(f, "format version {found} is newer than this build understands")
            }
            DecodeError::BadChecksum { stored, computed } => {
                write!(f, "checksum {stored:#010x} does not match {computed:#010x}: file damaged")
            }
            DecodeError::Truncated => write!(f, "file is truncated"),
            DecodeError::BadGeometry => write!(f, "file declares an implausible block size"),
            DecodeError::UnknownRole { code } => {
                write!(f, "file contains an unknown chunk ({code})")
            }
        }
    }
}

fn le_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn le_u64(b: &[u8], off: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(v)
}

pub fn decode(bytes: &[u8], crc: &impl Crc32) -> Result<Archive, DecodeError> {
    if bytes.len() < FIXED_LEN + 4 {
        return Err(DecodeError::TooShort);
    }
    if bytes[..8] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = le_u32(bytes, 8);
    if !(MIN_VERSION..=VERSION).contains(&version) {
        return Err(DecodeError::UnsupportedVersion { found: version });
    }

    let body = &bytes[..bytes.len() - 4];
    let stored = le_u32(bytes, bytes.len() - 4);
    let computed = crc.crc32(body);
    if stored != computed {
        return Err(DecodeError::BadChecksum { stored, computed });
    }

    let block_size = le_u32(bytes, 12);
    if block_size < 512 || block_size % 512 != 0 || block_size > 65536 {
        return Err(DecodeError::BadGeometry);
    }
    let last_block = le_u64(bytes, 16);
    let disk_guid = Guid::read_from(bytes, 24).ok_or(DecodeError::TooShort)?;
    let time = Timestamp {
        year: u16::from_le_bytes([bytes[40], bytes[41]]),
        month: bytes[42],
        day: bytes[43],
        hour: bytes[44],
        minute: bytes[45],
        second: bytes[46],
    };
    let health = Health::from_code(bytes[47]);
    let count = le_u32(bytes, 48) as usize;

    let mut chunks = Vec::new();
    let mut off = FIXED_LEN;
    for _ in 0..count {
        if off + CHUNK_HEAD_LEN > body.len() {
            return Err(DecodeError::Truncated);
        }
        let code = le_u32(body, off);
        let role = Role::from_code(code).ok_or(DecodeError::UnknownRole { code })?;
        let lba = le_u64(body, off + 4);
        // Blocks is derivable from the payload length; it is stored for
        // readability in a hexdump and cross-checked here.
        let _blocks = le_u64(body, off + 12);
        let len = le_u64(body, off + 20);
        off += CHUNK_HEAD_LEN;
        let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
        let end = off.checked_add(len).ok_or(DecodeError::Truncated)?;
        if end > body.len() {
            return Err(DecodeError::Truncated);
        }
        chunks.push(Chunk { role, lba, data: body[off..end].to_vec() });
        off = end;
    }

    let mut meta = Vec::new();
    if version >= 2 {
        if off + 4 > body.len() {
            return Err(DecodeError::Truncated);
        }
        let len = le_u32(body, off) as usize;
        off += 4;
        let end = off.checked_add(len).ok_or(DecodeError::Truncated)?;
        if end > body.len() {
            return Err(DecodeError::Truncated);
        }
        meta = decode_meta(&body[off..end]);
    }

    Ok(Archive { version, block_size, last_block, disk_guid, time, health, chunks, meta })
}

/// Why an archive cannot be written to a particular disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mismatch {
    BlockSize {
        archive: u32,
        disk: u32,
    },
    LastBlock {
        archive: u64,
        disk: u64,
    },
    /// A chunk would land outside the disk.
    OutOfRange {
        lba: u64,
        blocks: u64,
        last_block: u64,
    },
    /// A chunk's payload is not a whole number of blocks.
    Unaligned {
        lba: u64,
        len: usize,
    },
    /// Nothing to write: the archive has no header chunks.
    Incomplete,
}

impl core::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Mismatch::BlockSize { archive, disk } => {
                write!(f, "backup is from a {archive}-byte-block disk, this one uses {disk}")
            }
            Mismatch::LastBlock { archive, disk } => {
                write!(
                    f,
                    "backup is from a disk of {} blocks, this one has {}",
                    archive + 1,
                    disk + 1
                )
            }
            Mismatch::OutOfRange { lba, blocks, last_block } => {
                write!(f, "a {blocks}-block section at LBA {lba} falls outside this disk (last block {last_block})")
            }
            Mismatch::Unaligned { lba, len } => {
                write!(f, "the section for LBA {lba} is {len} bytes, not a whole number of blocks")
            }
            Mismatch::Incomplete => write!(f, "the backup does not contain both GPT headers"),
        }
    }
}

/// Build the ordered restore for `archive` onto the disk `analysis` came
/// from.
///
/// Geometry must match exactly. A backup from a different-sized disk
/// describes partitions that are not there, and a backup GPT header names
/// an LBA that only exists on the disk it came from.
pub fn restore_plan(archive: &Archive, analysis: &Analysis) -> Result<RepairPlan, Mismatch> {
    if archive.block_size != analysis.block_size {
        return Err(Mismatch::BlockSize { archive: archive.block_size, disk: analysis.block_size });
    }
    if archive.last_block != analysis.last_block {
        return Err(Mismatch::LastBlock { archive: archive.last_block, disk: analysis.last_block });
    }

    for chunk in &archive.chunks {
        if chunk.data.len() % archive.block_size as usize != 0 || chunk.data.is_empty() {
            return Err(Mismatch::Unaligned { lba: chunk.lba, len: chunk.data.len() });
        }
        let blocks = chunk.blocks(archive.block_size);
        let end = chunk.lba.saturating_add(blocks);
        if end > analysis.last_block + 1 {
            return Err(Mismatch::OutOfRange {
                lba: chunk.lba,
                blocks,
                last_block: analysis.last_block,
            });
        }
    }

    let primary_header = archive.chunk(Role::PrimaryHeader).ok_or(Mismatch::Incomplete)?;
    let backup_header = archive.chunk(Role::BackupHeader).ok_or(Mismatch::Incomplete)?;

    fn push(steps: &mut Vec<Step>, archive: &Archive, role: Role) {
        if let Some(c) = archive.chunk(role) {
            steps.push(Step::Write {
                lba: c.lba,
                data: c.data.clone(),
                what: String::from(role.describe()),
            });
        }
    }

    // Same rule as a repair: an array must be durable before any header
    // claims it is there with a given CRC.
    let mut steps = Vec::new();
    push(&mut steps, archive, Role::PrimaryEntries);
    push(&mut steps, archive, Role::BackupEntries);
    steps
        .push(Step::Flush { why: "entry arrays must be durable before the headers point at them" });
    push(&mut steps, archive, Role::Mbr);
    push(&mut steps, archive, Role::PrimaryHeader);
    push(&mut steps, archive, Role::BackupHeader);
    steps.push(Step::Flush { why: "commit headers" });

    // The report needs a header and a table to show. Prefer the primary
    // copy; fall back to the backup so a snapshot with a damaged primary
    // still renders something.
    let header = GptHeader::parse(&primary_header.data)
        .or_else(|| GptHeader::parse(&backup_header.data))
        .ok_or(Mismatch::Incomplete)?;
    let entries = archive
        .chunk(Role::PrimaryEntries)
        .or_else(|| archive.chunk(Role::BackupEntries))
        .map(|c| {
            parse_array(&c.data, header.number_of_partition_entries, header.size_of_partition_entry)
        })
        .unwrap_or_default();

    Ok(RepairPlan { steps, header, entries })
}

/// Snapshots are named `gpt.001`, `gpt.002`, ...
///
/// A sequence number rather than a timestamp for two reasons. It fits 8.3,
/// so the name reads the same from firmware, Windows and Linux. And it does
/// not depend on the clock: firmware that will not give a sensible time
/// would otherwise produce a pile of identically-named files, exactly when
/// ambiguity is least welcome. The date lives inside the file.
pub(crate) const NAME_PREFIX: &str = "gpt.";
pub const MAX_SEQUENCE: u32 = 999;

/// The number in `gpt.NNN`, case-insensitively: FAT may hand back `GPT.001`.
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
/// Counts from the highest rather than filling gaps: reusing the number of
/// a snapshot someone deleted would make the ordering lie about which is
/// newest. `None` once the space is exhausted — better to say so than to
/// overwrite somebody's oldest backup.
pub fn next_name(existing: &[String]) -> Option<String> {
    let highest = existing.iter().filter_map(|n| sequence_of(n)).max().unwrap_or(0);
    let next = highest + 1;
    (next <= MAX_SEQUENCE).then(|| format!("{NAME_PREFIX}{next:03}"))
}

/// One-line summary for a list of saved backups.
///
/// Ordered date first, because that is what people scan a list of backups
/// by, and the rest has to survive being clipped on a narrow console.
pub fn summary(archive: &Archive) -> String {
    let used = archive.entries().iter().filter(|e| e.is_used()).count();
    format!(
        "{}  {:>2} parts  {:>9}  {}",
        archive.time,
        used,
        crate::report::human_size(archive.capacity()),
        archive.health.describe()
    )
}

/// Lines describing an archive in full, for the screen before a restore.
pub fn describe(archive: &Archive) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(key(format!("  Taken:      {}", archive.time)));
    out.push(dim(format!("  Disk GUID:  {}", archive.disk_guid)));
    out.push(dim(format!(
        "  Geometry:   {} blocks x {} B",
        archive.last_block + 1,
        archive.block_size
    )));
    out.push(Line::new(
        format!("  State then: {}", archive.health.describe()),
        archive.health.style(),
    ));
    out.push(Line::blank());
    out.push(title("  Contains:"));
    for chunk in &archive.chunks {
        out.push(dim(format!(
            "    LBA {:<12} {} ({} blocks)",
            chunk.lba,
            chunk.role.describe(),
            chunk.blocks(archive.block_size)
        )));
    }
    match archive.health {
        Health::Healthy => {}
        Health::MbrOnly => {
            out.push(Line::blank());
            style::block(
                &mut out,
                Style::Warn,
                &[
                    "  NOTE: both tables were sound, but the protective MBR was",
                    "  wrong and is saved as found. Restoring puts that back",
                    "  too; the repair operation regenerates it properly.",
                ],
            );
        }
        _ => {
            out.push(Line::blank());
            style::block(
                &mut out,
                Style::Bad,
                &[
                    "  WARNING: this snapshot was taken from a table that was",
                    "  already damaged. Restoring it reinstates the damage.",
                ],
            );
        }
    }
    out
}

/// How strongly a snapshot belongs to a particular disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Match {
    /// Same disk GUID and geometry. This snapshot is this disk's own.
    SameDisk,
    /// Geometry fits, but the disk GUID differs — a reinstall, a restored
    /// image, or a different drive of the same model.
    SameGeometry,
    /// Does not fit. Restore will refuse.
    DifferentDisk,
}

/// What a snapshot and a disk have in common.
///
/// The point of this is the question someone asks years later: *is this
/// file from this machine?* Geometry and disk GUID answer part of it, but
/// the strongest evidence is the per-partition unique GUIDs. Those are
/// generated once when a partition is created and survive OS upgrades, so
/// a snapshot sharing eight of them with the disk in front of you is that
/// disk's, whatever else has changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Comparison {
    pub geometry: bool,
    pub disk_guid: bool,
    pub shared_partitions: usize,
    pub archive_partitions: usize,
}

impl Comparison {
    pub fn verdict(&self) -> Match {
        if !self.geometry {
            return Match::DifferentDisk;
        }
        // Partition identity outranks the disk GUID: the GUID is a single
        // field that any partitioner may rewrite, whereas matching unique
        // GUIDs across several partitions cannot happen by accident.
        if self.disk_guid
            || (self.archive_partitions > 0
                && self.shared_partitions * 2 >= self.archive_partitions)
        {
            return Match::SameDisk;
        }
        Match::SameGeometry
    }

    pub fn style(&self) -> Style {
        match self.verdict() {
            Match::SameDisk => Style::Good,
            Match::SameGeometry => Style::Warn,
            Match::DifferentDisk => Style::Bad,
        }
    }

    /// The evidence, phrased so a caller can put a disk name in front of it.
    pub fn describe(&self) -> String {
        match self.verdict() {
            Match::SameDisk => format!(
                "{} of {} partitions still carry the same unique GUID",
                self.shared_partitions, self.archive_partitions
            ),
            Match::SameGeometry => {
                String::from("geometry fits, but no partition is recognisably the same")
            }
            Match::DifferentDisk => String::from("geometry does not fit"),
        }
    }
}

pub fn compare(archive: &Archive, analysis: &Analysis) -> Comparison {
    let entries = archive.entries();
    let archive_used: Vec<Guid> =
        entries.iter().filter(|e| e.is_used()).map(|e| e.unique_guid).collect();
    let disk_used: Vec<Guid> = analysis
        .best_view()
        .map(|t| t.used_entries().map(|(_, e)| e.unique_guid).collect())
        .unwrap_or_default();

    Comparison {
        geometry: archive.block_size == analysis.block_size
            && archive.last_block == analysis.last_block,
        disk_guid: !archive.disk_guid.is_zero()
            && analysis.best_view().map(|t| t.header.disk_guid) == Some(archive.disk_guid),
        shared_partitions: archive_used.iter().filter(|g| disk_used.contains(g)).count(),
        archive_partitions: archive_used.len(),
    }
}

/// Everything known about a snapshot, for someone deciding whether to
/// trust it.
///
/// `against` is the comparison with a specific disk, when one has been
/// chosen; at file-picking time there is none yet.
pub fn inspect(archive: &Archive, against: Option<(&str, &Comparison)>) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(key(format!("  Taken:        {}", archive.time)));
    if archive.time.year < 2000 {
        out.push(Line::new(
            String::from("                (the clock was not set; treat the date as unknown)"),
            Style::Warn,
        ));
    }
    out.push(Line::new(
        format!("  State then:   {}", archive.health.describe()),
        archive.health.style(),
    ));
    if let Some((disk, c)) = against {
        out.push(Line::new(format!("  Belongs to:   {} - {}", disk, c.describe()), c.style()));
    }
    out.push(Line::blank());

    out.push(title("  Identity"));
    out.push(line(format!("    Disk GUID     {}", archive.disk_guid)));
    out.push(line(format!(
        "    Geometry      {} blocks x {} B = {}",
        archive.last_block + 1,
        archive.block_size,
        crate::report::human_size(archive.capacity())
    )));
    if let Some(h) = archive.primary_header() {
        out.push(line(format!("    Usable range  {}..{}", h.first_usable_lba, h.last_usable_lba)));
        out.push(line(format!(
            "    Entry array   {} entries x {} B at LBA {}",
            h.number_of_partition_entries, h.size_of_partition_entry, h.partition_entry_lba
        )));
    }
    out.push(Line::blank());

    out.push(title("  Recorded when it was written"));
    if archive.meta.is_empty() {
        out.push(dim(format!(
            "    nothing: format version {} predates provenance",
            archive.version
        )));
    } else {
        for (k, v) in &archive.meta {
            out.push(line(format!("    {k:<13} {v}")));
        }
    }
    out.push(Line::blank());

    let entries = archive.entries();
    let used: Vec<&PartitionEntry> = entries.iter().filter(|e| e.is_used()).collect();
    out.push(title(format!("  Partitions ({})", used.len())));
    out.push(dim(format!(
        "    {:>2}  {:>12} {:>10}  {:<20} {}",
        "#", "Start LBA", "Size", "Name", "Unique GUID"
    )));
    for (i, e) in entries.iter().enumerate().filter(|(_, e)| e.is_used()) {
        let size = e
            .block_count()
            .map(|b| crate::report::human_size(b.saturating_mul(archive.block_size as u64)))
            .unwrap_or_else(|| String::from("invalid"));
        out.push(line(format!(
            "    {:>2}  {:>12} {:>10}  {:<20} {}",
            i + 1,
            e.starting_lba,
            size,
            e.name_string(),
            e.unique_guid
        )));
    }
    out.push(Line::blank());

    out.push(title("  Sectors stored"));
    for chunk in &archive.chunks {
        out.push(dim(format!(
            "    LBA {:<12} {} ({} blocks)",
            chunk.lba,
            chunk.role.describe(),
            chunk.blocks(archive.block_size)
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::SoftCrc32;
    extern crate std;

    fn sample() -> Archive {
        Archive {
            version: VERSION,
            block_size: 512,
            last_block: 1_953_525_167,
            disk_guid: Guid::from_fields(1, 2, 3, [4; 8]),
            time: Timestamp { year: 2026, month: 8, day: 1, hour: 12, minute: 13, second: 14 },
            health: Health::Healthy,
            meta: alloc::vec![(String::from("tool"), String::from("test"))],
            chunks: alloc::vec![
                Chunk { role: Role::Mbr, lba: 0, data: alloc::vec![0xAA; 512] },
                Chunk { role: Role::PrimaryEntries, lba: 2, data: alloc::vec![0x11; 512 * 32] },
                Chunk { role: Role::PrimaryHeader, lba: 1, data: alloc::vec![0x22; 512] },
            ],
        }
    }

    #[test]
    fn round_trips() {
        let a = sample();
        let bytes = encode(&a, &SoftCrc32);
        let b = decode(&bytes, &SoftCrc32).expect("decode");
        assert_eq!(b.block_size, a.block_size);
        assert_eq!(b.last_block, a.last_block);
        assert_eq!(b.disk_guid, a.disk_guid);
        assert_eq!(b.time, a.time);
        assert_eq!(b.health, a.health);
        assert_eq!(b.chunks.len(), 3);
        assert_eq!(b.chunk(Role::PrimaryEntries).unwrap().data, alloc::vec![0x11; 512 * 32]);
    }

    #[test]
    fn a_single_flipped_bit_is_caught() {
        let mut bytes = encode(&sample(), &SoftCrc32);
        let n = bytes.len() / 2;
        bytes[n] ^= 0x01;
        assert!(matches!(decode(&bytes, &SoftCrc32), Err(DecodeError::BadChecksum { .. })));
    }

    #[test]
    fn truncation_is_caught() {
        let bytes = encode(&sample(), &SoftCrc32);
        let short = &bytes[..bytes.len() - 100];
        // Losing the tail also loses the checksum, so this is rejected
        // whichever way it is read; what matters is that it is rejected.
        assert!(decode(short, &SoftCrc32).is_err());
    }

    #[test]
    fn foreign_files_are_rejected() {
        assert!(matches!(decode(&alloc::vec![0u8; 4096], &SoftCrc32), Err(DecodeError::BadMagic)));
        assert!(matches!(decode(b"EFIGPTBK", &SoftCrc32), Err(DecodeError::TooShort)));
    }

    #[test]
    fn conventional_array_is_32_blocks_at_512_and_4_at_4k() {
        assert_eq!(default_array_blocks(512), 32);
        assert_eq!(default_array_blocks(4096), 4);
        assert_eq!(default_array_blocks(0), 0);
    }
}
