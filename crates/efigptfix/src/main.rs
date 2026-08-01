//! Repair a corrupt primary GPT from the backup, from the ESP, on a Deck
//! whose firmware still enumerates partitions via the backup table.
//!
//! Safety rules, in order of application:
//!
//! * whole disks only (`Media->LogicalPartition == FALSE`)
//! * never removable media, never read-only media
//! * never the device this image booted from, and nothing at all if that
//!   device cannot be identified
//! * never a disk carrying a hybrid MBR
//! * never a backup table that fails structural or layout sanity checks
//! * never without the operator entering the confirmation sequence
//!
//! The interface is a D-pad menu because the target hardware has no
//! keyboard; `docs/efiprobe-deck.log` records what its buttons actually
//! report.

#![no_std]
#![no_main]

extern crate alloc;

mod blockdev;
mod fwcrc;
mod selfdev;
mod ui;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use blockdev::UefiDisk;
use fwcrc::FirmwareCrc32;
use gptcore::repair::{analyze, apply, plan, Analysis, RepairPlan};
use gptcore::{prevent, report};
use selfdev::BootDevice;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::prelude::*;
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::proto::ProtocolPointer;
use uefi::Identify;

const CRC: FirmwareCrc32 = FirmwareCrc32;

/// Open a protocol without disturbing drivers that already hold it.
///
/// Inspection must not disconnect anything: an exclusive open on the disk
/// carrying our own ESP would tear the filesystem driver out from under the
/// running image.
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
            // Excluding by device path prefix also covers the case where the
            // ESP we booted from is a partition of this very disk.
            match get_protocol::<DevicePath>(*handle) {
                Ok(path) => !boot_device.covers(&path),
                // No device path means no way to prove this is not our own
                // boot disk, so leave it alone.
                Err(_) => false,
            }
        })
        .collect()
}

fn read_disk(handle: Handle) -> Option<Analysis> {
    let io = get_protocol::<BlockIO>(handle).ok()?;
    let mut disk = UefiDisk::new(io).ok()?;
    analyze(&mut disk, &CRC).ok()
}

/// Execute a plan against a disk opened exclusively for the purpose.
fn execute(handle: Handle, plan: &RepairPlan) -> Result<(), String> {
    // SAFETY: we are committed to writing by this point, and Exclusive is
    // what stops another driver's cached view racing the write.
    let opened = unsafe {
        boot::open_protocol::<BlockIO>(
            OpenProtocolParams { handle, agent: boot::image_handle(), controller: None },
            OpenProtocolAttributes::Exclusive,
        )
    };
    let io = opened.map_err(|_| "could not open the disk for exclusive access".to_string())?;
    let mut disk = UefiDisk::new(io).map_err(|e| e.to_string())?;
    apply(&mut disk, plan).map_err(|e| format!("write failed: {e}"))
}

fn disk_label(index: usize, handle: Handle) -> String {
    let path = get_protocol::<DevicePath>(handle).ok();
    format!("Disk {}: {}", index + 1, path_text(path.as_deref()))
}

fn no_disks_message(boot_device: &BootDevice) {
    let mut lines = alloc::vec![String::from("  No eligible fixed, non-boot disks were found.")];
    if !boot_device.is_known() {
        lines.push(String::new());
        lines.push(String::from("  The boot device could not be identified, so every disk"));
        lines.push(String::from("  is excluded rather than risk writing to the wrong one."));
    }
    ui::message("Nothing to do", &lines);
}

/// Scan every eligible disk and offer to repair the ones that need it.
fn run_repair(boot_device: &BootDevice) {
    let disks = candidate_disks(boot_device);
    if disks.is_empty() {
        no_disks_message(boot_device);
        return;
    }

    for (i, handle) in disks.iter().enumerate() {
        let label = disk_label(i, *handle);
        let Some(analysis) = read_disk(*handle) else {
            ui::message("Repair", &alloc::vec![label, String::from("  could not be read")]);
            continue;
        };

        let repair = plan(&analysis, &CRC);
        let mut lines = alloc::vec![label.clone(), String::new()];
        lines.extend(report::render(&analysis, repair.as_ref()));

        if !ui::page("Repair primary GPT", &lines) {
            continue; // B skips this disk
        }
        let Some(repair) = repair else {
            continue;
        };

        let warning = alloc::vec![
            format!("  {label}"),
            String::new(),
            String::from("  This REWRITES the partition table on this disk."),
            String::from("  The proposed table came from the backup GPT and was"),
            String::from("  shown on the previous screen."),
        ];
        if !ui::confirm_sequence("Authorise write", &warning) {
            ui::message("Repair", &alloc::vec![String::from("  Cancelled. Nothing was written.")]);
            continue;
        }

        match execute(*handle, &repair) {
            Ok(()) => ui::message(
                "Repair",
                &alloc::vec![
                    String::from("  Written and flushed."),
                    String::new(),
                    String::from("  Reboot when you are done."),
                ],
            ),
            Err(e) => ui::message(
                "Repair FAILED",
                &alloc::vec![
                    format!("  {e}"),
                    String::new(),
                    String::from("  Do NOT reboot into the OS; use a rescue USB."),
                ],
            ),
        }
    }
}

/// Offer to close the FirstUsableLBA gap on healthy disks.
fn run_prevent(boot_device: &BootDevice) {
    let disks = candidate_disks(boot_device);
    if disks.is_empty() {
        no_disks_message(boot_device);
        return;
    }

    for (i, handle) in disks.iter().enumerate() {
        let label = disk_label(i, *handle);
        let Some(analysis) = read_disk(*handle) else {
            continue;
        };

        let verdict = prevent::assess(&analysis);
        let mut lines = alloc::vec![label.clone(), String::new()];
        lines.extend(prevent::describe(verdict));

        if !ui::page("Prevent recurrence", &lines) {
            continue;
        }
        if !verdict.will_write() {
            continue;
        }
        let Some(gap_plan) = prevent::plan(&analysis, &CRC) else {
            continue;
        };

        let mut plan_lines = alloc::vec![label.clone(), String::new()];
        plan_lines.extend(report::render_plan(&gap_plan));
        if !ui::page("Prevent recurrence: what will be written", &plan_lines) {
            continue;
        }

        let warning = alloc::vec![
            format!("  {label}"),
            String::new(),
            String::from("  This modifies a HEALTHY partition table on a theory"),
            String::from("  about what corrupts it. No partition moves, and it is"),
            String::from("  reversible, but it is not a repair."),
        ];
        if !ui::confirm_sequence("Authorise write", &warning) {
            ui::message("Prevent", &alloc::vec![String::from("  Cancelled. Nothing was written.")]);
            continue;
        }

        match execute(*handle, &gap_plan) {
            Ok(()) => ui::message("Prevent", &alloc::vec![String::from("  Written and flushed.")]),
            Err(e) => ui::message(
                "Prevent FAILED",
                &alloc::vec![
                    format!("  {e}"),
                    String::new(),
                    String::from("  Check the disk with a rescue USB before rebooting."),
                ],
            ),
        }
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("failed to initialise uefi helpers");
    ui::hide_cursor();

    let boot_device = BootDevice::resolve();
    let mut intro = alloc::vec![format!("  version {}", env!("CARGO_PKG_VERSION"))];
    if boot_device.is_known() {
        intro.push(format!("  booted from {}", path_text(boot_device.path())));
        intro.push(String::from("  that device and the disk carrying it are excluded"));
    } else {
        intro.push(String::from("  BOOT DEVICE UNKNOWN - every disk is excluded"));
        intro.push(String::from("  nothing can be written in this state"));
    }

    loop {
        let choice = ui::menu(
            "efigptfix",
            &intro,
            &[
                "Scan and repair a corrupt primary GPT",
                "Prevent recurrence (close the FirstUsableLBA gap)",
                "Exit",
            ],
        );
        match choice {
            Some(0) => run_repair(&boot_device),
            Some(1) => run_prevent(&boot_device),
            _ => break,
        }
    }

    if fwcrc::used_fallback() {
        ui::message(
            "Note",
            &alloc::vec![
                String::from("  Firmware CalculateCrc32 was unavailable;"),
                String::from("  the built-in implementation was used instead.")
            ],
        );
    }
    ui::clear();
    Status::SUCCESS
}
