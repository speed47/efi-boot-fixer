//! Files on the volume this image was launched from.
//!
//! Backups are written next to the application, on the ESP, because that is
//! the one place guaranteed to exist in the scenario this tool is for: no
//! keyboard, no USB stick, the machine will not boot. A backup stored
//! anywhere else is a backup the operator cannot reach.
//!
//! Two consequences of that placement are handled by the caller rather than
//! here. Writing to a whole disk with `OpenProtocolAttributes::Exclusive`
//! disconnects the partition and filesystem drivers serving it, so any ESP
//! access must happen *before* a write to the disk carrying that ESP. And
//! an ESP on the disk being repaired is not independent storage: it is a
//! convenience, not an off-device backup, and the operator is told so.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use uefi::boot;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileInfo, FileMode};
use uefi::{cstr16, CString16, Status};

/// Where backups live, relative to the root of the ESP.
pub const DIR: &str = "EFIGPTFIX";

/// A saved file, read into memory in full.
pub struct Saved {
    pub name: String,
    /// The contents, or why they could not be read.
    ///
    /// A snapshot that cannot be read is still a snapshot that exists. It
    /// has to appear here rather than be dropped, both so the operator is
    /// told about it and so its name stays accounted for when the next
    /// snapshot is numbered.
    pub data: Result<Vec<u8>, String>,
}

fn err(what: &str, status: Status) -> String {
    format!("{what} ({status:?})")
}

/// Open `\EFIGPTFIX`, creating it if `create`.
///
/// `Ok(None)` means the directory is genuinely not there, which is the
/// normal state of a volume nothing has been backed up to yet. Every other
/// failure is an error and is reported as one: an ESP we cannot read is not
/// an ESP with no snapshots on it, and treating the two alike is how a
/// backup gets numbered over the top of one that already exists.
fn open_dir(create: bool) -> Result<Option<Directory>, String> {
    let mut fs = boot::get_image_file_system(boot::image_handle())
        .map_err(|e| err("no filesystem on the boot volume", e.status()))?;
    let mut root = fs.open_volume().map_err(|e| err("cannot open the ESP", e.status()))?;
    let mode = if create { FileMode::CreateReadWrite } else { FileMode::Read };
    let handle = match root.open(cstr16!("EFIGPTFIX"), mode, FileAttribute::DIRECTORY) {
        Ok(handle) => handle,
        Err(e) if e.status() == Status::NOT_FOUND => return Ok(None),
        Err(e) => return Err(err("cannot open \\EFIGPTFIX on the ESP", e.status())),
    };
    handle.into_directory().map(Some).ok_or_else(|| "\\EFIGPTFIX is not a directory".to_string())
}

fn name16(name: &str) -> Result<CString16, String> {
    CString16::try_from(name).map_err(|_| format!("{name} is not a usable filename"))
}

/// Write `data` to a *new* `\EFIGPTFIX\<name>`.
///
/// Refuses a name that is already taken, rather than replacing it. Callers
/// pick names from `backup::next_name`, which never reuses a sequence, so a
/// file already sitting here means the caller's view of the directory was
/// incomplete — and the one thing that must not happen then is destroying
/// the snapshot it could not see.
///
/// Refusing also disposes of a hazard the previous delete-then-create was
/// there to handle: `FileMode::CreateReadWrite` opens an existing file
/// without truncating it, so writing a shorter file over a longer one would
/// leave the tail of the old one attached. Nothing is written over now.
pub fn save(name: &str, data: &[u8]) -> Result<String, String> {
    let mut dir = open_dir(true)?.ok_or_else(|| format!("cannot create \\{DIR} on the ESP"))?;
    let name16 = name16(name)?;

    match dir.open(&name16, FileMode::Read, FileAttribute::empty()) {
        Ok(_) => return Err(format!("\\{DIR}\\{name} already exists; refusing to replace it")),
        Err(e) if e.status() == Status::NOT_FOUND => {}
        Err(e) => return Err(err("cannot tell whether the file already exists", e.status())),
    }

    let handle = dir
        .open(&name16, FileMode::CreateReadWrite, FileAttribute::empty())
        .map_err(|e| err("cannot create the backup file", e.status()))?;
    let mut file = handle.into_regular_file().ok_or_else(|| "not a regular file".to_string())?;
    file.write(data).map_err(|e| err("write failed", e.status()))?;
    file.flush().map_err(|e| err("flush failed", e.status()))?;
    Ok(format!("\\{DIR}\\{name}"))
}

/// Read every snapshot in `\EFIGPTFIX`, sorted by name.
///
/// The files are a few tens of KiB each, so reading them all up front is
/// cheaper than the alternative: the picker has to show what is *inside*
/// each one — when it was taken, from what geometry, whether the table was
/// healthy — and a filename cannot say that.
pub fn list() -> Result<Vec<Saved>, String> {
    // No directory means no backups, which is an answer, not a failure.
    // Anything else that went wrong is now an `Err` from `open_dir`.
    let Some(mut dir) = open_dir(false)? else {
        return Ok(Vec::new());
    };

    let mut names = Vec::new();
    loop {
        match dir.read_entry_boxed() {
            Ok(Some(info)) => {
                if info.attribute().contains(FileAttribute::DIRECTORY) {
                    continue;
                }
                // `gpt.NNN` is what this build writes; `*.bin` is what
                // earlier builds and tools/deck-corrupt.py write, and a
                // snapshot must not become invisible because the naming
                // scheme moved on.
                let name = info.file_name().to_string();
                let lower = name.to_ascii_lowercase();
                if lower.starts_with("gpt.") || lower.ends_with(".bin") {
                    names.push(name);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(err("cannot read \\EFIGPTFIX", e.status())),
        }
    }
    names.sort();

    // One unreadable file must not hide the others, so a failure here is
    // carried on the entry rather than aborting the listing. It is not
    // discarded: the name still counts as taken, and the caller reports it.
    Ok(names.into_iter().map(|name| Saved { data: read(&mut dir, &name), name }).collect())
}

fn read(dir: &mut Directory, name: &str) -> Result<Vec<u8>, String> {
    let name16 = name16(name)?;
    let handle = dir
        .open(&name16, FileMode::Read, FileAttribute::empty())
        .map_err(|e| err("cannot open", e.status()))?;
    let mut file = handle.into_regular_file().ok_or_else(|| "not a regular file".to_string())?;
    let info = file.get_boxed_info::<FileInfo>().map_err(|e| err("cannot stat", e.status()))?;
    let size = usize::try_from(info.file_size()).map_err(|_| "file too large".to_string())?;
    let mut buf = alloc::vec![0u8; size];
    let read = file.read(&mut buf).map_err(|e| err("read failed", e.status()))?;
    buf.truncate(read);
    Ok(buf)
}
