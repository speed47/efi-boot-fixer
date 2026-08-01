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
    pub data: Vec<u8>,
}

fn err(what: &str, status: Status) -> String {
    format!("{what} ({status:?})")
}

/// Open `\EFIGPTFIX`, creating it if absent.
fn open_dir(create: bool) -> Result<Directory, String> {
    let mut fs = boot::get_image_file_system(boot::image_handle())
        .map_err(|e| err("no filesystem on the boot volume", e.status()))?;
    let mut root = fs.open_volume().map_err(|e| err("cannot open the ESP", e.status()))?;
    let mode = if create { FileMode::CreateReadWrite } else { FileMode::Read };
    let handle = root
        .open(cstr16!("EFIGPTFIX"), mode, FileAttribute::DIRECTORY)
        .map_err(|e| err("cannot open \\EFIGPTFIX on the ESP", e.status()))?;
    handle.into_directory().ok_or_else(|| "\\EFIGPTFIX is not a directory".to_string())
}

fn name16(name: &str) -> Result<CString16, String> {
    CString16::try_from(name).map_err(|_| format!("{name} is not a usable filename"))
}

/// Write `data` to `\EFIGPTFIX\<name>`, replacing any existing file.
///
/// The delete-then-create is not superstition: `FileMode::CreateReadWrite`
/// opens an existing file without truncating it, so writing a shorter file
/// over a longer one leaves the tail of the old one attached.
pub fn save(name: &str, data: &[u8]) -> Result<String, String> {
    let mut dir = open_dir(true)?;
    let name16 = name16(name)?;

    if let Ok(existing) = dir.open(&name16, FileMode::ReadWrite, FileAttribute::empty()) {
        let _ = existing.delete();
    }

    let handle = dir
        .open(&name16, FileMode::CreateReadWrite, FileAttribute::empty())
        .map_err(|e| err("cannot create the backup file", e.status()))?;
    let mut file = handle.into_regular_file().ok_or_else(|| "not a regular file".to_string())?;
    file.write(data).map_err(|e| err("write failed", e.status()))?;
    file.flush().map_err(|e| err("flush failed", e.status()))?;
    Ok(format!("\\{DIR}\\{name}"))
}

/// Read every `*.bin` in `\EFIGPTFIX`, newest name last.
///
/// The files are a few tens of KiB each, so reading them all up front is
/// cheaper than the alternative: the picker has to show what is *inside*
/// each one — when it was taken, from what geometry, whether the table was
/// healthy — and a filename cannot say that.
pub fn list() -> Result<Vec<Saved>, String> {
    let mut dir = match open_dir(false) {
        Ok(d) => d,
        // No directory means no backups, which is an answer, not a failure.
        Err(_) => return Ok(Vec::new()),
    };

    let mut names = Vec::new();
    loop {
        match dir.read_entry_boxed() {
            Ok(Some(info)) => {
                if info.attribute().contains(FileAttribute::DIRECTORY) {
                    continue;
                }
                let name = info.file_name().to_string();
                if name.to_ascii_lowercase().ends_with(".bin") {
                    names.push(name);
                }
            }
            Ok(None) => break,
            Err(e) => return Err(err("cannot read \\EFIGPTFIX", e.status())),
        }
    }
    names.sort();

    let mut out = Vec::new();
    for name in names {
        match read(&mut dir, &name) {
            Ok(data) => out.push(Saved { name, data }),
            // One unreadable file must not hide the others.
            Err(_) => continue,
        }
    }
    Ok(out)
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
