//! Test scaffolding: real disk images, built and judged by sgdisk.
//!
//! The point of going through sgdisk rather than gptcore's own writer is
//! that a bug shared between our reader and our writer would otherwise be
//! invisible. sgdisk is the independent opinion.

#![allow(dead_code)]

use gptcore::disk::{BlockDevice, IoError};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const BLOCK_SIZE: u32 = 512;
/// Sparse, so the nominal size costs nothing on disk.
pub const IMAGE_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// A `BlockDevice` over an image file, recording writes so tests can
/// assert on ordering.
pub struct FileDisk {
    file: File,
    block_size: u32,
    blocks: u64,
    pub journal: Vec<Op>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Write { lba: u64, blocks: u64 },
    Flush,
}

impl FileDisk {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(FileDisk {
            file,
            block_size: BLOCK_SIZE,
            blocks: len / BLOCK_SIZE as u64,
            journal: Vec::new(),
        })
    }

    /// Open while pretending the device is smaller than the file, to model
    /// a backup GPT recovered onto a disk it does not fit.
    pub fn open_truncated(path: &Path, blocks: u64) -> std::io::Result<Self> {
        let mut d = Self::open(path)?;
        d.blocks = blocks;
        Ok(d)
    }

    pub fn writes(&self) -> Vec<Op> {
        self.journal.clone()
    }
}

impl BlockDevice for FileDisk {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn last_block(&self) -> u64 {
        self.blocks - 1
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
        if buf.len() % self.block_size as usize != 0 {
            return Err(IoError::Unaligned);
        }
        self.file.read_exact_at(buf, lba * self.block_size as u64).map_err(|_| IoError::DeviceError)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), IoError> {
        if buf.len() % self.block_size as usize != 0 {
            return Err(IoError::Unaligned);
        }
        self.file
            .write_all_at(buf, lba * self.block_size as u64)
            .map_err(|_| IoError::DeviceError)?;
        self.journal.push(Op::Write { lba, blocks: buf.len() as u64 / self.block_size as u64 });
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.file.sync_data().map_err(|_| IoError::DeviceError)?;
        self.journal.push(Op::Flush);
        Ok(())
    }
}

fn sgdisk() -> Command {
    let mut c = Command::new("sgdisk");
    // sgdisk lives in /usr/sbin, which is not always on a test PATH.
    let path = std::env::var("PATH").unwrap_or_default();
    c.env("PATH", format!("{path}:/usr/sbin:/sbin"));
    c
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("failed to run sgdisk - is gdisk installed?");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "sgdisk failed: {stderr}\n{stdout}");
    stdout
}

/// stdout and stderr together. sgdisk sends its corruption warnings to
/// stderr while stdout still says "No problems found" about the table it
/// silently loaded from the backup, so judging health on stdout alone
/// would call a wrecked primary healthy.
fn run_combined(cmd: &mut Command) -> String {
    let out = cmd.output().expect("failed to run sgdisk - is gdisk installed?");
    format!("{}{}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout))
}

/// Drop sgdisk warnings that do not indicate damage.
///
/// The real Deck's table was written by util-linux fdisk, which leaves a
/// gap between the end of the entry array (LBA 33) and the first usable
/// block (2048). sgdisk warns about it on a perfectly healthy disk, so
/// counting it as ill health would make every Deck fixture look corrupt.
fn strip_benign_warnings(out: &str) -> String {
    const BENIGN: &[&str] = &["There is a gap between the main partition table"];
    let mut keep = Vec::new();
    let mut skipping = false;
    for line in out.lines() {
        if BENIGN.iter().any(|m| line.contains(m)) {
            skipping = true;
            continue;
        }
        // The warning is a paragraph; it ends at the next blank line.
        if skipping {
            if line.trim().is_empty() {
                skipping = false;
            }
            continue;
        }
        keep.push(line);
    }
    keep.join("\n")
}

/// A temporary image that deletes itself.
pub struct Image {
    pub path: PathBuf,
    /// Total 512-byte sectors. Carried per image because the Deck fixture
    /// is a different size from the synthetic ones.
    pub sectors: u64,
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Image {
    pub fn disk(&self) -> FileDisk {
        FileDisk::open(&self.path).expect("open image")
    }

    /// `sgdisk -v`, the independent health check.
    pub fn verify(&self) -> String {
        run_combined(sgdisk().arg("-v").arg(&self.path))
    }

    /// True only if sgdisk read the on-disk primary without complaint.
    ///
    /// "No problems found" alone is not enough: sgdisk prints that after
    /// transparently falling back to the backup. A genuinely healthy disk
    /// produces no Caution/Warning/ERROR text at all.
    pub fn is_clean(&self) -> bool {
        let out = strip_benign_warnings(&self.verify());
        out.contains("No problems found")
            && !["Caution", "Warning", "ERROR", "corrupt"].iter().any(|m| out.contains(m))
    }

    /// True if sgdisk explicitly flagged the *main* header or table as bad.
    pub fn primary_flagged_bad(&self) -> bool {
        let out = self.verify();
        out.contains("Main header: ERROR")
            || out.contains("Main partition table: ERROR")
            || out.contains("invalid main GPT header")
    }

    /// The partition listing, used to prove a repair restored the table
    /// rather than merely producing something self-consistent.
    pub fn print(&self) -> String {
        let out = run(sgdisk().arg("-p").arg(&self.path));
        // Drop the header block, which mentions free space and can shift.
        out.lines()
            .skip_while(|l| !l.trim_start().starts_with("Number"))
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn read_lba(&self, lba: u64, count: u64) -> Vec<u8> {
        let f = File::open(&self.path).unwrap();
        let mut buf = vec![0u8; (count * BLOCK_SIZE as u64) as usize];
        f.read_exact_at(&mut buf, lba * BLOCK_SIZE as u64).unwrap();
        buf
    }

    pub fn write_lba(&self, lba: u64, data: &[u8]) {
        let f = OpenOptions::new().write(true).open(&self.path).unwrap();
        f.write_all_at(data, lba * BLOCK_SIZE as u64).unwrap();
    }

    pub fn zero_lba(&self, lba: u64, count: u64) {
        self.write_lba(lba, &vec![0u8; (count * BLOCK_SIZE as u64) as usize]);
    }

    pub fn last_block(&self) -> u64 {
        self.sectors - 1
    }
}

fn unique_path(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("gptcore-{tag}-{n}-{}.img", std::process::id()))
}

fn blank(tag: &str) -> Image {
    let path = unique_path(tag);
    let f = File::create(&path).expect("create image");
    f.set_len(IMAGE_BYTES).expect("sparse resize");
    drop(f);
    Image { path, sectors: IMAGE_BYTES / BLOCK_SIZE as u64 }
}

/// Rebuild a full-size sparse image from the committed Steam Deck sector
/// dumps in `tests/data/deck`.
///
/// These are real sectors from a dual-booting Deck, with the disk GUID and
/// the per-partition unique GUIDs replaced by obvious placeholders and the
/// CRCs resealed. Type GUIDs, names and extents are untouched, because
/// those are what the layout checks actually key on.
pub fn deck_image() -> Image {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/deck");
    let head = std::fs::read(dir.join("head.bin")).expect("tests/data/deck/head.bin");
    let tail = std::fs::read(dir.join("tail.bin")).expect("tests/data/deck/tail.bin");
    let sectors: u64 = std::fs::read_to_string(dir.join("sectors.txt"))
        .expect("tests/data/deck/sectors.txt")
        .trim()
        .parse()
        .expect("sector count");

    let path = unique_path("deck");
    let f = File::create(&path).expect("create image");
    f.set_len(sectors * BLOCK_SIZE as u64).expect("sparse resize");
    f.write_all_at(&head, 0).expect("write head");
    let tail_lba = sectors - (tail.len() as u64 / BLOCK_SIZE as u64);
    f.write_all_at(&tail, tail_lba * BLOCK_SIZE as u64).expect("write tail");
    drop(f);
    Image { path, sectors }
}

/// The stock SteamOS 3.x A/B layout plus the Windows partitions a
/// dual-booting Deck carries.
pub fn steamos_image() -> Image {
    let img = blank("steamos");
    run(sgdisk().arg("-o").arg(&img.path));
    run(sgdisk()
        .args(["-n", "1:2048:+256M", "-t", "1:ef00", "-c", "1:esp"])
        .args(["-n", "2:0:+64M", "-t", "2:0700", "-c", "2:efi-A"])
        .args(["-n", "3:0:+64M", "-t", "3:0700", "-c", "3:efi-B"])
        .args(["-n", "4:0:+5G", "-t", "4:8304", "-c", "4:rootfs-A"])
        .args(["-n", "5:0:+5G", "-t", "5:8304", "-c", "5:rootfs-B"])
        .args(["-n", "6:0:+256M", "-t", "6:8310", "-c", "6:var-A"])
        .args(["-n", "7:0:+256M", "-t", "7:8310", "-c", "7:var-B"])
        .args(["-n", "8:0:+10G", "-t", "8:8302", "-c", "8:home"])
        .args(["-n", "9:0:+16M", "-t", "9:2700"])
        .args(["-n", "10:0:+20G", "-t", "10:0700", "-c", "10:Basic data partition"])
        .arg(&img.path));
    img
}

/// A disk that is emphatically not a SteamOS install.
pub fn foreign_image() -> Image {
    let img = blank("foreign");
    run(sgdisk().arg("-o").arg(&img.path));
    run(sgdisk()
        .args(["-n", "1:2048:+8G", "-t", "1:0700", "-c", "1:some other disk"])
        .arg(&img.path));
    img
}
