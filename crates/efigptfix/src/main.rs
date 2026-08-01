//! Repair a corrupt primary GPT from the backup, from the ESP, on a Deck
//! whose firmware still enumerates partitions via the backup table.
//!
//! Safety rules, in order of application:
//!
//! * whole disks only (`Media->LogicalPartition == FALSE`)
//! * never removable media
//! * never the device this image booted from, and nothing at all if that
//!   device cannot be identified
//! * never a disk carrying a hybrid MBR
//! * never a backup table that fails structural or layout sanity checks
//! * never without the operator typing the confirmation word

#![no_std]
#![no_main]

extern crate alloc;

mod blockdev;
mod fwcrc;
mod selfdev;
mod ui;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use blockdev::UefiDisk;
use fwcrc::FirmwareCrc32;
use gptcore::repair::{analyze, apply, plan, Verdict};
use selfdev::BootDevice;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::prelude::*;
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::proto::ProtocolPointer;
use uefi::Identify;

const CONFIRM_WORD: &str = "REPAIR";
const CRC: FirmwareCrc32 = FirmwareCrc32;

/// Open a protocol without disturbing drivers that already hold it.
///
/// Inspection must not disconnect anything: an exclusive open on the disk
/// carrying our own ESP would tear the filesystem driver out from under
/// the running image.
fn get_protocol<P: ProtocolPointer + ?Sized>(handle: Handle) -> uefi::Result<ScopedProtocol<P>> {
    // SAFETY: GetProtocol neither installs nor removes interfaces, and the
    // returned ScopedProtocol closes the protocol when dropped.
    unsafe {
        boot::open_protocol::<P>(
            OpenProtocolParams { handle, agent: boot::image_handle(), controller: None },
            OpenProtocolAttributes::GetProtocol,
        )
    }
}

fn path_text(path: Option<&DevicePath>) -> String {
    let Some(path) = path else {
        return "<unknown>".to_string();
    };
    match path.to_string(DisplayOnly(true), AllowShortcuts(false)) {
        Ok(text) => text.to_string(),
        Err(_) => "<unprintable device path>".to_string(),
    }
}

/// Whole, fixed, writable disks that are not the one we booted from.
fn candidate_disks(boot_device: &BootDevice) -> Vec<Handle> {
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&BlockIO::GUID)) else {
        return Vec::new();
    };

    handles
        .iter()
        .copied()
        .filter(|handle| {
            let Ok(io) = get_protocol::<BlockIO>(*handle) else {
                return false;
            };
            let media = io.media();
            if media.is_logical_partition() || media.is_removable_media() {
                return false;
            }
            if !media.is_media_present() || media.is_read_only() {
                return false;
            }
            // Excluding by device path prefix also covers the case where
            // the ESP we booted from is a partition of this very disk.
            match get_protocol::<DevicePath>(*handle) {
                Ok(path) => !boot_device.covers(&path),
                // No device path means no way to prove this is not our own
                // boot disk, so leave it alone.
                Err(_) => false,
            }
        })
        .collect()
}

/// Inspect one disk, and repair it if the operator confirms.
/// Returns true if anything was written.
fn handle_disk(index: usize, handle: Handle) -> bool {
    let path = get_protocol::<DevicePath>(handle).ok();
    uefi::println!("Disk {index}: {}", path_text(path.as_deref()));
    drop(path);

    let Ok(io) = get_protocol::<BlockIO>(handle) else {
        uefi::println!("  cannot open BlockIO, skipping");
        return false;
    };
    let mut disk = match UefiDisk::new(io) {
        Ok(d) => d,
        Err(why) => {
            uefi::println!("  skipping: {why}");
            return false;
        }
    };

    let analysis = match analyze(&mut disk, &CRC) {
        Ok(a) => a,
        Err(e) => {
            uefi::println!("  read failed: {e}");
            return false;
        }
    };
    drop(disk);

    ui::print_analysis(&analysis);
    uefi::println!("  => {}", ui::verdict_line(analysis.verdict));

    if !analysis.verdict.will_write() {
        uefi::println!();
        return false;
    }
    let Some(repair) = plan(&analysis, &CRC) else {
        uefi::println!();
        return false;
    };

    if analysis.verdict == Verdict::PrimaryRepairable {
        ui::print_table(&repair, analysis.block_size);
    }
    ui::print_plan(&repair);
    uefi::println!();

    if !ui::confirm(CONFIRM_WORD) {
        uefi::println!("  skipped, nothing written");
        uefi::println!();
        return false;
    }

    // Re-open exclusively only now that we are committed, so the write is
    // not racing another driver's cached view of the table.
    let opened = unsafe {
        boot::open_protocol::<BlockIO>(
            OpenProtocolParams { handle, agent: boot::image_handle(), controller: None },
            OpenProtocolAttributes::Exclusive,
        )
    };

    let wrote = match opened {
        Err(_) => {
            uefi::println!("  could not open the disk for exclusive access, nothing written");
            false
        }
        Ok(io) => match UefiDisk::new(io) {
            Err(why) => {
                uefi::println!("  {why}, nothing written");
                false
            }
            Ok(mut target) => match apply(&mut target, &repair) {
                Ok(()) => {
                    uefi::println!("  repair written and flushed.");
                    true
                }
                Err(e) => {
                    uefi::println!("  WRITE FAILED ({e})");
                    uefi::println!("  Do NOT reboot into the OS; use a rescue USB.");
                    false
                }
            },
        },
    };
    uefi::println!();
    wrote
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("failed to initialise uefi helpers");
    ui::banner();

    let boot_device = BootDevice::resolve();
    if boot_device.is_known() {
        uefi::println!("Booted from : {}", path_text(boot_device.path()));
        uefi::println!("That device and the disk carrying it are excluded.");
    } else {
        uefi::println!("Could not identify the boot device.");
        uefi::println!("Refusing to consider any disk; nothing will be written.");
    }
    uefi::println!();

    let disks = candidate_disks(&boot_device);
    if disks.is_empty() {
        uefi::println!("No eligible fixed, non-boot disks found.");
        uefi::println!();
    }

    let mut repaired = 0usize;
    for (i, handle) in disks.iter().enumerate() {
        if handle_disk(i + 1, *handle) {
            repaired += 1;
        }
    }

    if fwcrc::used_fallback() {
        uefi::println!("Note: firmware CalculateCrc32 was unavailable; built-in CRC used.");
    }
    if repaired > 0 {
        uefi::println!("{repaired} disk(s) repaired. Reboot now.");
    } else {
        uefi::println!("Nothing was written.");
    }

    uefi::println!();
    uefi::println!("Press Enter to exit.");
    ui::wait_for_enter();

    Status::SUCCESS
}
