//! Where the tool keeps its own files.
//!
//! The default is the volume this image was launched from — the ESP —
//! because that is the one place guaranteed to exist in the scenario this
//! tool is for: no keyboard, no USB stick, the machine will not boot. A
//! backup stored somewhere the operator cannot reach is not a backup.
//!
//! That guarantee is also the limit of it. An ESP on the disk being backed
//! up is not independent storage, so when removable media is attached it is
//! offered as a second destination and the ESP becomes the default rather
//! than the only choice. Both are [`Volume`]s here and nothing above this
//! module treats one differently from the other; what differs is only which
//! ones are offered, and that is [`removable`]'s answer.
//!
//! Two consequences of the ESP placement are handled by the caller rather
//! than here. Writing to a whole disk with
//! `OpenProtocolAttributes::Exclusive` disconnects the partition and
//! filesystem drivers serving it, so any file access must happen *before* a
//! write to the disk carrying that volume. And an ESP on the disk being
//! repaired is a convenience, not an off-device backup, and the operator is
//! told so.
//!
//! This is not [`crate::espscan`], which is read-only, looks at every ESP
//! the firmware can see, and is about somebody else's bootloaders.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use uefi::boot::{self, ScopedProtocol, SearchType};
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::{
    Directory, File, FileAttribute, FileInfo, FileMode, FileSystemInfo,
};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{cstr16, CString16, Handle, Identify, Status};

/// Declare the backup directory's name once, in both spellings it is
/// needed in.
///
/// `cstr16!` takes a literal and produces UCS-2, which is what the file
/// protocol wants; every message shown to an operator wants a `&str`. Two
/// hand-written copies of the same name is one edit away from a build that
/// opens one directory and names another in its errors, so the literal
/// appears exactly once and both constants come from it.
macro_rules! backup_dir {
    ($name:literal) => {
        /// Where backups live, relative to the root of a volume.
        pub const DIR: &str = $name;
        /// [`DIR`] as UEFI wants it.
        const DIR16: &uefi::CStr16 = cstr16!($name);
    };
}

backup_dir!("BOOTFIXR");

/// What the launch volume is called on screen.
///
/// Lower case because it is read in the middle of a sentence as often as at
/// the start of a line: "on the ESP this program was launched from".
const BOOT_VOLUME: &str = "the ESP this program was launched from";

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

/// A volume the tool can keep files on.
pub struct Volume {
    /// The filesystem's handle, or `None` for the volume this image came
    /// from.
    ///
    /// The launch volume is deliberately reached through
    /// `get_image_file_system` rather than by a handle we resolved
    /// ourselves: that is the firmware's own answer to "where did this
    /// image come from", and it is the path that has to keep working when
    /// everything else about the machine is broken.
    handle: Option<Handle>,
    /// What the operator is told they are choosing.
    pub name: String,
    /// Its device path, which is what tells two identical sticks apart.
    pub path: String,
    /// Bytes free, when the filesystem would say.
    pub free: Option<u64>,
    pub removable: bool,
}

impl Volume {
    /// The volume this image was launched from.
    pub fn boot() -> Volume {
        let handle = image_volume();
        let device = handle.and_then(|h| crate::get_protocol::<DevicePath>(h).ok());
        let mut volume = Volume {
            handle: None,
            name: String::from(BOOT_VOLUME),
            path: crate::path_text(device.as_deref()),
            free: None,
            // Asked of the media rather than assumed, because running this
            // from a rescue stick is a supported way to use it: a copy
            // written there is already off-device, and callers decide what
            // to say to the operator from this flag.
            removable: handle.is_some_and(is_removable),
        };
        drop(device);
        // Best effort, and a failure is not disqualifying here the way it
        // is for a removable volume: the ESP is the fallback destination,
        // and a filesystem that will not report its free space is still one
        // that will very probably take a 40 KiB file.
        volume.free = volume.info().ok().and_then(|(_, free)| free);
        volume
    }

    fn fs(&self) -> Result<ScopedProtocol<SimpleFileSystem>, String> {
        match self.handle {
            None => boot::get_image_file_system(boot::image_handle())
                .map_err(|e| err("no filesystem on the boot volume", e.status())),
            Some(handle) => crate::get_protocol::<SimpleFileSystem>(handle)
                .map_err(|e| err("no filesystem on this volume", e.status())),
        }
    }

    /// What the filesystem says about itself: its label, and its free space.
    ///
    /// Also the first thing that proves a volume can be opened at all,
    /// which is why [`removable`] drops the ones that fail it. A
    /// destination that cannot be read is not a destination that can be
    /// written, and finding that out while the choice is being offered
    /// beats finding it out after the operator has made it.
    fn info(&self) -> Result<(Option<String>, Option<u64>), String> {
        let mut fs = self.fs()?;
        let mut root = fs.open_volume().map_err(|e| err("cannot open the volume", e.status()))?;
        // A driver that will not answer is not a refusal: the label and the
        // free space are both decoration.
        let Ok(info) = root.get_boxed_info::<FileSystemInfo>() else {
            return Ok((None, None));
        };
        if info.read_only() {
            return Err(String::from("mounted read-only"));
        }
        let label = info.volume_label().to_string();
        let label = label.trim().to_string();
        Ok(((!label.is_empty()).then_some(label), Some(info.free_space())))
    }

    /// Open the backup directory, creating it if `create`.
    ///
    /// `Ok(None)` means the directory is genuinely not there, which is the
    /// normal state of a volume nothing has been backed up to yet. Every
    /// other failure is an error and is reported as one: a volume we cannot
    /// read is not a volume with no snapshots on it, and treating the two
    /// alike is how a backup gets numbered over the top of one that already
    /// exists.
    fn open_dir(&self, create: bool) -> Result<Option<Directory>, String> {
        let mut fs = self.fs()?;
        let mut root = fs.open_volume().map_err(|e| err("cannot open the volume", e.status()))?;
        let mode = if create { FileMode::CreateReadWrite } else { FileMode::Read };
        let handle = match root.open(DIR16, mode, FileAttribute::DIRECTORY) {
            Ok(handle) => handle,
            Err(e) if e.status() == Status::NOT_FOUND => return Ok(None),
            Err(e) => return Err(err(&format!("cannot open \\{DIR}"), e.status())),
        };
        handle.into_directory().map(Some).ok_or_else(|| format!("\\{DIR} is not a directory"))
    }

    /// Write `data` to a *new* file in the backup directory.
    ///
    /// Refuses a name that is already taken, rather than replacing it.
    /// Callers pick names from `backup::next_name`, which never reuses a
    /// sequence, so a file already sitting here means the caller's view of
    /// the directory was incomplete — and the one thing that must not
    /// happen then is destroying the snapshot it could not see.
    ///
    /// Refusing also disposes of a hazard the previous delete-then-create
    /// was there to handle: `FileMode::CreateReadWrite` opens an existing
    /// file without truncating it, so writing a shorter file over a longer
    /// one would leave the tail of the old one attached. Nothing is written
    /// over now.
    pub fn save(&self, name: &str, data: &[u8]) -> Result<String, String> {
        let mut dir = self.open_dir(true)?.ok_or_else(|| format!("cannot create \\{DIR}"))?;
        let name16 = name16(name)?;

        match dir.open(&name16, FileMode::Read, FileAttribute::empty()) {
            Ok(_) => return Err(format!("\\{DIR}\\{name} already exists; refusing to replace it")),
            Err(e) if e.status() == Status::NOT_FOUND => {}
            Err(e) => return Err(err("cannot tell whether the file already exists", e.status())),
        }

        let handle = dir
            .open(&name16, FileMode::CreateReadWrite, FileAttribute::empty())
            .map_err(|e| err("cannot create the backup file", e.status()))?;
        let mut file =
            handle.into_regular_file().ok_or_else(|| "not a regular file".to_string())?;
        file.write(data).map_err(|e| err("write failed", e.status()))?;
        file.flush().map_err(|e| err("flush failed", e.status()))?;
        Ok(format!("\\{DIR}\\{name}"))
    }

    /// Every filename in the backup directory, sorted.
    ///
    /// Names only. [`Volume::list`] reads each file because the GPT picker
    /// has to show what is inside one; choosing the next snapshot number
    /// does not, and the boot snapshots share this directory with the GPT
    /// ones. A name that is present but unreadable still counts as taken,
    /// which is the whole point of numbering from a directory listing
    /// rather than from a counter.
    ///
    /// So does a *directory*. [`Volume::save`] decides a name is free by
    /// asking the firmware to open it, and a directory opens, so one named
    /// `gpt-002.bkp` occupies that name as far as the only test that
    /// matters is concerned. Filtering directories out here — as the
    /// listing that decodes files rightly does — would hand `save` a name
    /// it then refuses, in this session and every session after it.
    pub fn names(&self) -> Result<Vec<String>, String> {
        let Some(mut dir) = self.open_dir(false)? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        loop {
            match dir.read_entry_boxed() {
                Ok(Some(info)) => names.push(info.file_name().to_string()),
                Ok(None) => break,
                Err(e) => return Err(err(&format!("cannot read \\{DIR}"), e.status())),
            }
        }
        names.sort();
        Ok(names)
    }

    /// Read every GPT snapshot in the backup directory, sorted by name.
    ///
    /// The files are a few tens of KiB each, so reading them all up front
    /// is cheaper than the alternative: the picker has to show what is
    /// *inside* each one — when it was taken, from what geometry, whether
    /// the table was healthy — and a filename cannot say that.
    pub fn list(&self) -> Result<Vec<Saved>, String> {
        self.list_matching(|name| gptcore::backup::sequence_of(name).is_some())
    }

    /// Read every boot configuration snapshot, sorted by name.
    ///
    /// Separate from [`Volume::list`] because the two kinds share a
    /// directory and nothing else: a picker offering to write a boot
    /// snapshot onto a disk, or a partition table into NVRAM, would be a
    /// picker with a bug in it.
    pub fn list_boot(&self) -> Result<Vec<Saved>, String> {
        self.list_matching(|name| gptcore::bootcfg::sequence_of(name).is_some())
    }

    /// The files in the backup directory whose lowercased name `wanted`
    /// accepts.
    fn list_matching(&self, wanted: impl Fn(&str) -> bool) -> Result<Vec<Saved>, String> {
        // No directory means no backups, which is an answer, not a failure.
        // Anything else that went wrong is now an `Err` from `open_dir`.
        let Some(mut dir) = self.open_dir(false)? else {
            return Ok(Vec::new());
        };

        let mut names = Vec::new();
        loop {
            match dir.read_entry_boxed() {
                Ok(Some(info)) => {
                    if info.attribute().contains(FileAttribute::DIRECTORY) {
                        continue;
                    }
                    let name = info.file_name().to_string();
                    if wanted(&name.to_ascii_lowercase()) {
                        names.push(name);
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(err(&format!("cannot read \\{DIR}"), e.status())),
            }
        }
        names.sort();

        // One unreadable file must not hide the others, so a failure here
        // is carried on the entry rather than aborting the listing. It is
        // not discarded: the name still counts as taken, and the caller
        // reports it.
        let mut total = 0usize;
        Ok(names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                if i >= MAX_FILES || total >= MAX_TOTAL {
                    return Saved {
                        name,
                        data: Err(format!(
                            "not read: \\{DIR}\\ holds more than {MAX_FILES} files or \
                             {} MiB of them",
                            MAX_TOTAL >> 20
                        )),
                    };
                }
                let data = read(&mut dir, &name);
                total += data.as_ref().map_or(0, Vec::len);
                Saved { name, data }
            })
            .collect())
    }
}

/// Every removable volume a backup could be written to.
///
/// Not "every removable ESP". A stick formatted by whatever was to hand is
/// exactly what somebody has in a drawer, and refusing it because its
/// partition type GUID is not the one for an ESP would repeat, in the other
/// direction, the mistake this tool has already made once — see `AGENTS.md`
/// on matching by name rather than by type GUID. Anything with a filesystem
/// on removable, writable, present media is offered.
///
/// The volume this image was launched from is excluded even when it is
/// itself removable, which running from a rescue stick makes normal: it is
/// already offered as the launch volume, and listing it twice would write
/// two numbered copies of one snapshot into one directory.
pub fn removable() -> Vec<Volume> {
    let launched_from = image_volume();
    let mut out = Vec::new();
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleFileSystem::GUID))
    else {
        return out;
    };

    for handle in handles.iter().copied() {
        if Some(handle) == launched_from {
            continue;
        }
        let Ok(io) = crate::get_protocol::<BlockIO>(handle) else {
            continue;
        };
        let media = io.media();
        let usable =
            media.is_removable_media() && media.is_media_present() && !media.is_read_only();
        drop(io);
        if !usable {
            continue;
        }

        let device = crate::get_protocol::<DevicePath>(handle).ok();
        let mut volume = Volume {
            handle: Some(handle),
            name: String::new(),
            path: crate::path_text(device.as_deref()),
            free: None,
            removable: true,
        };
        drop(device);

        // Named from the filesystem where it has a label, since that is
        // what somebody with two sticks in front of them recognises.
        let Ok((label, free)) = volume.info() else {
            continue;
        };
        volume.name = label.unwrap_or_default();
        volume.free = free;
        out.push(volume);
    }

    // The firmware's handle order is not something to show an operator as
    // if it meant anything, and two sweeps of the same machine should list
    // the same sticks in the same order.
    out.sort_by(|a, b| a.path.cmp(&b.path));

    // Numbered after the sort, so the numbers run the way the list does.
    // Two unlabelled sticks would otherwise be two identical rows, which is
    // exactly the choice nobody can make.
    let mut unlabelled = 0;
    for volume in out.iter_mut() {
        if volume.name.is_empty() {
            unlabelled += 1;
            volume.name = format!("Removable volume {unlabelled}");
        }
    }
    out
}

/// The handle of the volume this image was launched from.
///
/// Used to keep that volume out of [`removable`] and to ask what kind of
/// media it sits on; the volume itself is opened through
/// `get_image_file_system`, never through this.
fn image_volume() -> Option<Handle> {
    boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?.device()
}

/// Whether this handle's media is removable, as far as the firmware says.
///
/// A partition handle inherits the answer from the device it is on, so this
/// needs no walk up to the whole disk.
fn is_removable(handle: Handle) -> bool {
    crate::get_protocol::<BlockIO>(handle).is_ok_and(|io| io.media().is_removable_media())
}

fn name16(name: &str) -> Result<CString16, String> {
    CString16::try_from(name).map_err(|_| format!("{name} is not a usable filename"))
}

/// The largest file worth reading into memory as a snapshot.
///
/// Two orders of magnitude above the ~34 KiB a 512-byte-block disk produces,
/// so nothing this tool writes can approach it. It exists because the
/// listing now sweeps removable media the operator keeps their own files
/// on: a coincidental namesake of a few hundred MiB would otherwise be read
/// in full merely to be rejected, and an allocation the firmware refuses is
/// not a rejection line but the `no_std` allocation error handler.
const MAX_FILE: u64 = 4 << 20;

/// And how much of the directory is worth holding at once.
///
/// [`MAX_FILE`] bounds one file; these bound the set, because
/// [`Volume::list_matching`] reads every match into memory before anything
/// is decoded. The naming scheme allows a thousand matches per volume, so
/// the per-file cap alone permits gigabytes — and the failure is not a
/// rejection line but the allocation error handler. A file past either
/// bound is still listed, with the reason in place of its contents, so
/// the screen says what it skipped and the name still counts as taken.
const MAX_FILES: usize = 64;
const MAX_TOTAL: usize = 16 << 20;

fn read(dir: &mut Directory, name: &str) -> Result<Vec<u8>, String> {
    let name16 = name16(name)?;
    let handle = dir
        .open(&name16, FileMode::Read, FileAttribute::empty())
        .map_err(|e| err("cannot open", e.status()))?;
    let mut file = handle.into_regular_file().ok_or_else(|| "not a regular file".to_string())?;
    let info = file.get_boxed_info::<FileInfo>().map_err(|e| err("cannot stat", e.status()))?;
    if info.file_size() > MAX_FILE {
        return Err(format!("{} bytes; too large to be a snapshot", info.file_size()));
    }
    let size = usize::try_from(info.file_size()).map_err(|_| "file too large".to_string())?;
    let mut buf = alloc::vec![0u8; size];
    let read = file.read(&mut buf).map_err(|e| err("read failed", e.status()))?;
    buf.truncate(read);
    Ok(buf)
}
