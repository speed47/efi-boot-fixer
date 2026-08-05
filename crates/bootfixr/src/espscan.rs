//! Finding bootloaders on the EFI System Partitions.
//!
//! This is not [`crate::esp`]. That module reads and writes the tool's own
//! backups on the volume the image was launched from, deliberately and
//! only there. This one is read-only and looks at every ESP the firmware
//! can see, because the loader that stopped being bootable is quite often
//! not on the volume you booted the rescue tool from.
//!
//! ESPs on fixed disks only, matching the refusal list in
//! `docs/safety.md`: a bootloader on a USB stick is a boot entry that
//! breaks when the stick is pulled.
//!
//! What counts as a bootloader is **two explicit lists, not a guessed
//! vendor table**. The tool has made that mistake once already, in the
//! other direction — an earlier version matched partitions by a guessed
//! type-GUID table and refused to repair the exact disk it was written for
//! (see `AGENTS.md`). So every `.efi` file found is reported; the table
//! only decides what it is *called*, and an unrecognised one is listed as
//! unrecognised rather than dropped.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use gptcore::Guid;
use uefi::boot::{self, SearchType};
use uefi::proto::device_path::build::{self, DevicePathBuilder};
use uefi::proto::device_path::media::{FilePath, HardDrive, PartitionSignature};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::media::partition::{GptPartitionType, PartitionInfo};
use uefi::{CString16, Handle, Identify};

/// What a filename means, when we recognise it.
const KNOWN: &[(&str, &str)] = &[
    ("bootx64.efi", "default loader"),
    ("steamcl.efi", "SteamOS chainloader"),
    ("shimx64.efi", "shim (Secure Boot)"),
    ("mmx64.efi", "MokManager"),
    ("grubx64.efi", "GRUB"),
    ("systemd-bootx64.efi", "systemd-boot"),
    ("refind_x64.efi", "rEFInd"),
    ("cloverx64.efi", "Clover"),
    ("bootmgfw.efi", "Windows Boot Manager"),
    // This tool. Named here because the operator most likely to be reading
    // this list is the one deciding where to install it, or looking for the
    // copy they installed last time.
    ("bootfixr.efi", "EFI Boot Fixer (this program)"),
];

fn describe_file(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    KNOWN.iter().find(|(f, _)| *f == lower).map(|(_, d)| *d).unwrap_or("unrecognised EFI binary")
}

/// One EFI System Partition, on a fixed disk.
pub struct EspVolume {
    /// The partition handle, kept so a boot entry's device path can be
    /// built from the firmware's own path to this volume rather than from
    /// one this tool assembled out of guesses.
    pub handle: Handle,
    /// The partition's device path, rendered.
    pub path: String,
    /// Its unique partition GUID, which is how a boot entry names it.
    pub partition: Guid,
    /// This is the volume the running image was launched from.
    pub boot: bool,
}

/// A bootable-looking file found on one of them.
pub struct Candidate {
    /// Index into [`Scan::volumes`].
    pub esp: usize,
    /// Path on that volume, e.g. `\EFI\steamos\steamcl.efi`.
    pub file: String,
    pub kind: &'static str,
    pub size: u64,
    /// The `Boot####` slot already pointing at this file, if there is one.
    pub registered: Option<u16>,
}

pub struct Scan {
    pub volumes: Vec<EspVolume>,
    pub candidates: Vec<Candidate>,
    /// Indices into `volumes` that looked like an ESP but could not be
    /// read. By index rather than by path: `path_text` answers `<unknown>`
    /// for a partition with no device path protocol, and two of those would
    /// otherwise be indistinguishable.
    pub unreadable: Vec<usize>,
    /// A directory sat deeper than [`MAX_DEPTH`] and was not descended
    /// into. Carried so the screens can say "there may be more", because a
    /// loader the scan silently missed reads as a loader that is gone.
    pub truncated: bool,
}

/// The pair that identifies a bootloader: which partition, and which file.
///
/// Comparing whole device paths byte for byte would be too strict —
/// firmware stores both full paths and the short `HD()/File()` form for the
/// same target, and a full path carries the controller topology in front.
/// The partition GUID and the file name are what actually pin down a
/// binary, and they survive both spellings.
fn target_of(path: &DevicePath) -> Option<(Guid, String)> {
    let mut partition = None;
    let mut file = None;

    for node in path.node_iter() {
        if let Ok(hd) = <&HardDrive>::try_from(node) {
            if let PartitionSignature::Guid(guid) = hd.partition_signature() {
                partition = Some(Guid(guid.to_bytes()));
            }
        }
        if let Ok(fp) = <&FilePath>::try_from(node) {
            let units = fp.path_name().to_vec();
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            file = Some(
                char::decode_utf16(units[..end].iter().copied())
                    .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect::<String>(),
            );
        }
    }
    Some((partition?, file?))
}

/// The same pair, from the raw bytes of a `Boot####`'s device path.
///
/// Bytes that do not parse as a device path yield `None` and the entry
/// simply matches nothing — which is also the honest answer for the
/// entries that have no hard-drive node at all: a PXE boot, the firmware's
/// own setup application, the built-in shell.
pub fn target_of_bytes(bytes: &[u8]) -> Option<(Guid, String)> {
    // Validating conversion, so nothing here reads past the slice even
    // when the bytes came from a variable store that truncated them.
    target_of(<&DevicePath>::try_from(bytes).ok()?)
}

fn name16(name: &str) -> Option<CString16> {
    CString16::try_from(name).ok()
}

/// The device path a boot entry for `file` on `volume` should hold.
///
/// Built by taking the firmware's *own* path to the partition and adding a
/// file node, rather than assembling one from a partition GUID. The
/// firmware knows how it reaches that disk — which controller, which
/// namespace — and a path this tool invented would have to guess that
/// topology and would be wrong on the first machine that differed.
///
/// The end-of-path marker is dropped before appending, since it is what
/// says the path stops at the partition.
pub fn boot_path(volume: Handle, file: &str) -> Result<Vec<u8>, String> {
    let file16 = name16(file).ok_or_else(|| format!("{file} is not a usable path"))?;
    let path = crate::get_protocol::<DevicePath>(volume)
        .map_err(|_| String::from("the firmware has no device path for this ESP"))?;

    let mut buf = Vec::new();
    let mut builder = DevicePathBuilder::with_vec(&mut buf);
    for node in path.node_iter() {
        if node.is_end_entire() {
            continue;
        }
        builder = builder.push(&node).map_err(|_| String::from("device path is too long"))?;
    }
    let built = builder
        .push(&build::media::FilePath { path_name: &file16 })
        .map_err(|_| String::from("device path is too long"))?
        .finalize()
        .map_err(|_| String::from("the device path could not be assembled"))?;

    Ok(built.as_bytes().to_vec())
}

/// `dir\name`, without doubling the separator when `dir` is the root.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "\\" {
        format!("\\{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// How deep under the root the walk descends.
///
/// Deep enough for anything a vendor actually ships —
/// `\EFI\Microsoft\Boot\bootmgfw.efi` is three levels — with room to
/// spare, while still bounding the recursion against a corrupt filesystem.
/// Hitting the limit is recorded, never silent: a loader the scan missed
/// without saying so reads as a loader that is gone.
const MAX_DEPTH: u32 = 8;

/// Every `.efi` file under `dir` (with its size, taken from the directory
/// walk itself), and under its subdirectories up to `depth` levels further
/// down. Sets `truncated` when a directory was left unvisited.
fn efi_files_recursive(
    root: &mut Directory,
    dir: &str,
    depth: u32,
    out: &mut Vec<(String, u64)>,
    truncated: &mut bool,
) {
    let Some(name) = name16(dir) else {
        return;
    };
    let Ok(handle) = root.open(&name, FileMode::Read, FileAttribute::empty()) else {
        return;
    };
    let Some(mut handle) = handle.into_directory() else {
        return;
    };

    let mut subdirs = Vec::new();
    while let Ok(Some(info)) = handle.read_entry_boxed() {
        let file = info.file_name().to_string();
        if info.attribute().contains(FileAttribute::DIRECTORY) {
            if file == "." || file == ".." {
                continue;
            }
            if depth > 0 {
                subdirs.push(file);
            } else {
                *truncated = true;
            }
            continue;
        }
        if file.to_ascii_lowercase().ends_with(".efi") {
            out.push((join_path(dir, &file), info.file_size()));
        }
    }
    // Dropped before recursing, so at most one directory handle from this
    // volume is open at a time.
    drop(handle);

    for subdir in subdirs {
        efi_files_recursive(root, &join_path(dir, &subdir), depth - 1, out, truncated);
    }
}

/// Everything bootable-looking on one volume, and whether any directory
/// was too deep to look into.
fn probe(root: &mut Directory) -> (Vec<(String, u64)>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    efi_files_recursive(root, "\\", MAX_DEPTH, &mut out, &mut truncated);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    (out, truncated)
}

/// Find every ESP on a fixed disk and everything bootable-looking on it.
///
/// `registered` is resolved against `entries`, which the caller has already
/// read from NVRAM: pairs of (slot, device path bytes).
pub fn scan(boot: &crate::selfdev::BootDevice, entries: &[(u16, Vec<u8>)]) -> Scan {
    let known: Vec<(u16, Guid, String)> = entries
        .iter()
        .filter_map(|(slot, bytes)| target_of_bytes(bytes).map(|(guid, file)| (*slot, guid, file)))
        .collect();

    let mut scan = Scan {
        volumes: Vec::new(),
        candidates: Vec::new(),
        unreadable: Vec::new(),
        truncated: false,
    };
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleFileSystem::GUID))
    else {
        return scan;
    };

    for handle in handles.iter().copied() {
        // An ESP by the definition, not by the firmware's opinion of it:
        // the partition type GUID is what the GPT actually says.
        let Ok(info) = crate::get_protocol::<PartitionInfo>(handle) else {
            continue;
        };
        let Some(gpt) = info.gpt_partition_entry() else {
            continue;
        };
        // `GptPartitionEntry` is packed, so both fields are copied out
        // rather than referenced.
        if { gpt.partition_type_guid } != GptPartitionType::EFI_SYSTEM_PARTITION {
            continue;
        }
        let partition = Guid({ gpt.unique_partition_guid }.to_bytes());
        drop(info);

        // Removable is inherited by the partition handle from its media,
        // so this needs no walk up to the whole disk.
        if let Ok(io) = crate::get_protocol::<BlockIO>(handle) {
            if io.media().is_removable_media() {
                continue;
            }
        }

        let device_path = crate::get_protocol::<DevicePath>(handle).ok();
        let volume = EspVolume {
            handle,
            path: crate::path_text(device_path.as_deref()),
            partition,
            boot: device_path.as_deref().is_some_and(|p| boot.covers(p)),
        };
        drop(device_path);

        let index = scan.volumes.len();
        let found = match crate::get_protocol::<SimpleFileSystem>(handle)
            .ok()
            .and_then(|mut fs| fs.open_volume().ok())
        {
            Some(mut root) => {
                let (found, truncated) = probe(&mut root);
                scan.truncated |= truncated;
                found
            }
            None => {
                scan.unreadable.push(index);
                scan.volumes.push(volume);
                continue;
            }
        };

        for (file, size) in found {
            let registered = known
                .iter()
                .find(|(_, guid, path)| *guid == partition && path.eq_ignore_ascii_case(&file))
                .map(|(slot, _, _)| *slot);
            scan.candidates.push(Candidate {
                esp: index,
                kind: describe_file(file.rsplit('\\').next().unwrap_or(&file)),
                file,
                size,
                registered,
            });
        }
        scan.volumes.push(volume);
    }
    scan
}
