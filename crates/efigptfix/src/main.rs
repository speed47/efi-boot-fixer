//! Inspect, repair, back up and restore GPTs from the firmware, on a
//! machine with no keyboard.
//!
//! The tool is meant to live on the Steam Deck's own ESP and be launched
//! from the firmware's "boot from file" menu, so that recovering from a
//! wrecked partition table needs neither a USB stick nor a keyboard. It
//! also runs perfectly well from a stick; the boot volume is reported, not
//! treated specially.
//!
//! Safety rules, in order of application:
//!
//! * whole disks only (`Media->LogicalPartition == FALSE`)
//! * never removable media, never read-only media
//! * never a disk carrying a hybrid MBR
//! * never a backup table that fails structural or layout sanity checks
//! * never a saved backup whose geometry does not match the disk
//! * never a write without the operator entering the confirmation sequence
//!
//! Note what is *not* on that list any more: the disk this image booted
//! from. Excluding it made the tool useless for its actual purpose. It is
//! labelled `[boot]` in the picker instead.
//!
//! The interface is a D-pad menu because the target hardware has no
//! keyboard; `docs/efiprobe-deck.log` records what its buttons actually
//! report.

#![no_std]
#![no_main]

extern crate alloc;

mod blockdev;
mod diskinfo;
mod esp;
mod fwcrc;
mod selfdev;
mod ui;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use blockdev::UefiDisk;
use fwcrc::FirmwareCrc32;
use gptcore::backup::{self, Timestamp};
use gptcore::repair::{analyze, apply, plan, Analysis, RepairPlan};
use gptcore::{layout, prevent, report};
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

fn now() -> Timestamp {
    match uefi::runtime::get_time() {
        Ok(t) => Timestamp {
            year: t.year(),
            month: t.month(),
            day: t.day(),
            hour: t.hour(),
            minute: t.minute(),
            second: t.second(),
        },
        // A machine whose RTC the firmware will not read still deserves
        // working backups; the file just gets a zero timestamp.
        Err(_) => Timestamp::default(),
    }
}

/// One candidate disk, as summarised for the picker.
struct Disk {
    handle: Handle,
    number: usize,
    path: String,
    /// This disk carries the volume the image was loaded from.
    boot: bool,
    model: Option<String>,
    block_size: u32,
    last_block: u64,
    steamos: bool,
    /// `None` if the disk could not be read at all.
    verdict: Option<gptcore::Verdict>,
}

impl Disk {
    fn capacity(&self) -> u64 {
        (self.last_block + 1).saturating_mul(self.block_size as u64)
    }

    /// The one line the operator picks from. Capacity first because it is
    /// what people actually recognise a drive by.
    fn label(&self) -> String {
        let mut s = format!("Disk {}  {:>10}", self.number, report::human_size(self.capacity()));
        if self.boot {
            s.push_str("  [boot]");
        }
        if self.steamos {
            s.push_str("  [SteamOS]");
        }
        if let Some(model) = &self.model {
            s.push_str("  ");
            s.push_str(model);
        }
        s
    }

    fn detail(&self) -> Vec<String> {
        let health = match self.verdict {
            Some(v) => report::verdict_line(v).to_string(),
            None => "could not be read".to_string(),
        };
        alloc::vec![
            format!("  {}", self.path),
            format!("  {} blocks x {} B", self.last_block + 1, self.block_size),
            format!("  GPT: {health}"),
        ]
    }
}

fn open_disk(handle: Handle) -> Result<UefiDisk, String> {
    let io = get_protocol::<BlockIO>(handle).map_err(|e| format!("cannot open disk ({e})"))?;
    UefiDisk::new(io).map_err(|e| e.to_string())
}

fn read_disk(handle: Handle) -> Result<(UefiDisk, Analysis), String> {
    let mut disk = open_disk(handle)?;
    let analysis = analyze(&mut disk, &CRC).map_err(|e| format!("cannot read the GPT ({e})"))?;
    Ok((disk, analysis))
}

/// Every whole, fixed, writable disk the firmware knows about.
///
/// Removable media stays out: the point of the exercise is the internal
/// drive, and a rescue stick that the tool was launched from is not
/// something anyone wants to find in a list of repair targets.
fn scan(boot_device: &BootDevice) -> Vec<Disk> {
    let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&BlockIO::GUID)) else {
        return Vec::new();
    };

    let mut disks = Vec::new();
    for handle in handles.iter().copied() {
        let Ok(io) = get_protocol::<BlockIO>(handle) else {
            continue;
        };
        let media = io.media();
        if media.is_logical_partition() || media.is_removable_media() {
            continue;
        }
        if !media.is_media_present() || media.is_read_only() {
            continue;
        }
        let block_size = media.block_size();
        let last_block = media.last_block();
        drop(io);

        let path = get_protocol::<DevicePath>(handle).ok();
        let boot = path.as_deref().is_some_and(|p| boot_device.covers(p));

        let (steamos, verdict) = match read_disk(handle) {
            Ok((_, analysis)) => {
                let hint = analysis
                    .best_view()
                    .map(|t| layout::steamos_hint(&t.entries))
                    .unwrap_or_default();
                (hint.likely(), Some(analysis.verdict))
            }
            Err(_) => (false, None),
        };

        disks.push(Disk {
            handle,
            number: disks.len() + 1,
            path: path_text(path.as_deref()),
            boot,
            model: diskinfo::model(handle),
            block_size,
            last_block,
            steamos,
            verdict,
        });
    }
    disks
}

/// Choose a disk, or `None` if the operator backed out.
fn pick_disk(title: &str, boot_device: &BootDevice) -> Option<Disk> {
    let disks = scan(boot_device);
    if disks.is_empty() {
        ui::message(
            "Nothing to do",
            &alloc::vec![
                String::from("  No fixed, writable, whole disks were found."),
                String::new(),
                String::from("  Only removable media is present, and this tool"),
                String::from("  does not offer to rewrite that."),
            ],
        );
        return None;
    }

    let intro = alloc::vec![
        String::from("  Choose a disk. Removable media is not listed."),
        String::from("  [boot] carries the volume this program came from."),
    ];
    let items: Vec<ui::Item> =
        disks.iter().map(|d| ui::Item::with_detail(d.label(), d.detail())).collect();
    let choice = ui::menu(title, &intro, &items, "B = back")?;
    disks.into_iter().nth(choice)
}

/// Execute a plan against the disk, exclusively if the firmware allows it.
///
/// Exclusive is what stops another driver's cached view racing the write,
/// but it also disconnects the partition and filesystem drivers serving the
/// disk — which, when the target is the disk we booted from, the firmware
/// may refuse outright. Falling back to a shared open is better than being
/// unable to repair the machine's only drive; the caller reports which
/// happened, and either way the advice is to reboot.
fn execute(disk: &Disk, plan: &RepairPlan) -> Result<bool, String> {
    // SAFETY: we are committed to writing by this point.
    let exclusive = unsafe {
        boot::open_protocol::<BlockIO>(
            OpenProtocolParams {
                handle: disk.handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::Exclusive,
        )
    };

    let (io, was_exclusive) = match exclusive {
        Ok(io) => (io, true),
        Err(_) => (
            get_protocol::<BlockIO>(disk.handle)
                .map_err(|_| "could not open the disk for writing".to_string())?,
            false,
        ),
    };

    let mut dev = UefiDisk::new(io).map_err(|e| e.to_string())?;
    apply(&mut dev, plan).map_err(|e| format!("write failed: {e}"))?;
    Ok(was_exclusive)
}

fn report_write(title: &str, disk: &Disk, result: Result<bool, String>, esp_lost: &mut bool) {
    match result {
        Ok(exclusive) => {
            if disk.boot && exclusive {
                *esp_lost = true;
            }
            let mut lines = alloc::vec![
                String::from("  Written and flushed."),
                String::new(),
                String::from("  Reboot when you are done."),
            ];
            if !exclusive {
                lines.push(String::new());
                lines.push(String::from("  (The disk was busy, so the write was made without"));
                lines.push(String::from("  exclusive access. The firmware's own view of the"));
                lines.push(String::from("  partitions is stale until you reboot.)"));
            }
            ui::message(title, &lines);
        }
        Err(e) => ui::message(
            &format!("{title} FAILED"),
            &alloc::vec![
                format!("  {e}"),
                String::new(),
                String::from("  Do NOT reboot into the OS; use a rescue USB."),
            ],
        ),
    }
}

fn show_error(title: &str, message: String) {
    ui::message(title, &alloc::vec![format!("  {message}")]);
}

/// Read-only: say what is wrong, write nothing, offer nothing.
fn run_check(boot_device: &BootDevice) {
    let Some(disk) = pick_disk("Check GPT", boot_device) else {
        return;
    };
    let (_, analysis) = match read_disk(disk.handle) {
        Ok(v) => v,
        Err(e) => return show_error("Check GPT", e),
    };

    let mut lines =
        alloc::vec![format!("  {}", disk.label()), format!("  {}", disk.path), String::new()];
    lines.extend(report::render_analysis(&analysis));
    if let Some(view) = analysis.best_view() {
        let caption =
            if view.is_valid() { "Current table:" } else { "Table as far as it could be read:" };
        lines.extend(report::render_entries(
            &view.entries,
            analysis.block_size,
            view.header.disk_guid,
            caption,
        ));
    }
    lines.push(String::new());
    lines.push(String::from("  Nothing was written. This check never modifies a disk."));
    ui::page("Check GPT (read only)", &lines);
}

/// Rebuild a corrupt primary GPT from the backup table.
fn run_repair(boot_device: &BootDevice, esp_lost: &mut bool) {
    let Some(disk) = pick_disk("Repair primary GPT", boot_device) else {
        return;
    };
    let (_, analysis) = match read_disk(disk.handle) {
        Ok(v) => v,
        Err(e) => return show_error("Repair", e),
    };

    let repair = plan(&analysis, &CRC);
    let mut lines = alloc::vec![format!("  {}", disk.label()), String::new()];
    lines.extend(report::render(&analysis, repair.as_ref()));

    if !ui::page("Repair primary GPT", &lines) {
        return;
    }
    let Some(repair) = repair else {
        return;
    };

    // Say what is actually about to happen. "Rewrites the partition table"
    // is alarming and, when only the protective MBR is wrong, untrue.
    let warning = if analysis.verdict == gptcore::Verdict::MbrOnly {
        alloc::vec![
            format!("  {}", disk.label()),
            String::new(),
            String::from("  This rewrites the protective MBR on this disk."),
            String::from("  Both GPTs are intact and are left alone."),
        ]
    } else {
        alloc::vec![
            format!("  {}", disk.label()),
            String::new(),
            String::from("  This REWRITES the partition table on this disk."),
            String::from("  The proposed table came from the backup GPT and was"),
            String::from("  shown on the previous screen."),
        ]
    };
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_error("Repair", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &repair);
    report_write("Repair", &disk, result, esp_lost);
}

/// Save both GPTs to the ESP this image came from.
fn run_backup(boot_device: &BootDevice, esp_lost: bool) {
    if esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    let Some(disk) = pick_disk("Back up GPT", boot_device) else {
        return;
    };
    let (mut dev, analysis) = match read_disk(disk.handle) {
        Ok(v) => v,
        Err(e) => return show_error("Back up GPT", e),
    };

    let archive = match backup::capture(&mut dev, &analysis, now()) {
        Ok(a) => a,
        Err(e) => return show_error("Back up GPT", format!("could not read the tables ({e})")),
    };
    let bytes = backup::encode(&archive, &CRC);

    let mut lines = alloc::vec![format!("  {}", disk.label()), String::new()];
    lines.extend(backup::describe(&archive));
    lines.push(String::new());
    lines.push(format!("  {} bytes will be written to \\{}\\ on the ESP", bytes.len(), esp::DIR));
    if disk.boot {
        lines.push(String::new());
        lines.push(String::from("  NOTE: the ESP is on this same disk, so this is a"));
        lines.push(String::from("  convenience copy, not an off-device backup. Copy the"));
        lines.push(String::from("  file elsewhere once you can boot again."));
    }
    if !ui::page("Back up GPT", &lines) {
        return;
    }

    // Writing a file needs no confirmation sequence: it creates data, it
    // does not overwrite a partition table.
    match esp::save(&filename(&archive), &bytes) {
        Ok(path) => ui::message(
            "Back up GPT",
            &alloc::vec![
                String::from("  Saved to the ESP as:"),
                format!("    {path}"),
                String::new(),
                String::from("  Restore reads it from there."),
            ],
        ),
        Err(e) => show_error("Back up GPT FAILED", e),
    }
}

/// A name that sorts chronologically and never silently replaces an
/// existing snapshot.
fn filename(archive: &backup::Archive) -> String {
    let base = format!("gpt-{}", archive.time.stamp());
    let taken: Vec<String> = esp::list().unwrap_or_default().into_iter().map(|s| s.name).collect();
    let mut name = format!("{base}.bin");
    let mut n = 2;
    while taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        name = format!("{base}-{n}.bin");
        n += 1;
    }
    name
}

fn warn_esp_may_be_gone() -> bool {
    ui::page(
        "The ESP may no longer be reachable",
        &alloc::vec![
            String::from("  A disk was written to with exclusive access during this"),
            String::from("  session, which disconnects the firmware's filesystem"),
            String::from("  driver for that disk."),
            String::new(),
            String::from("  Reading or writing files on the ESP may fail until you"),
            String::from("  reboot. Continue anyway?"),
        ],
    )
}

/// Put a saved snapshot back.
fn run_restore(boot_device: &BootDevice, esp_lost: &mut bool) {
    if *esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    let saved = match esp::list() {
        Ok(s) => s,
        Err(e) => return show_error("Restore GPT", e),
    };
    if saved.is_empty() {
        return show_error(
            "Restore GPT",
            format!("No backups found in \\{}\\ on the ESP.", esp::DIR),
        );
    }

    // Decode first. A file that will not parse must never be offered as a
    // choice: a damaged snapshot is exactly what must not reach a disk.
    let mut usable: Vec<(String, backup::Archive)> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for file in saved {
        match backup::decode(&file.data, &CRC) {
            Ok(a) => usable.push((file.name, a)),
            Err(e) => rejected.push(format!("  {} - {e}", file.name)),
        }
    }
    if usable.is_empty() {
        let mut lines = alloc::vec![String::from("  No usable backup files on the ESP.")];
        if !rejected.is_empty() {
            lines.push(String::new());
            lines.push(String::from("  Rejected:"));
            lines.extend(rejected);
        }
        return ui::message("Restore GPT", &lines);
    }
    if !rejected.is_empty() {
        let mut lines = alloc::vec![String::from("  Some files could not be read and are not")];
        lines.push(String::from("  offered below:"));
        lines.push(String::new());
        lines.extend(rejected);
        ui::message("Restore GPT", &lines);
    }

    let items: Vec<ui::Item> = usable
        .iter()
        .map(|(name, a)| {
            ui::Item::with_detail(
                format!("{name}   {}", backup::summary(a)),
                alloc::vec![
                    format!("  disk GUID {}", a.disk_guid),
                    format!("  taken {}", a.time),
                    format!("  state then: {}", a.health.describe()),
                ],
            )
        })
        .collect();
    let intro = alloc::vec![
        format!("  Backups found in \\{}\\ on the ESP.", esp::DIR),
        String::from("  A backup only fits the disk it was taken from."),
    ];
    let Some(choice) = ui::menu("Restore GPT", &intro, &items, "B = back") else {
        return;
    };
    let archive = &usable[choice].1;

    let Some(disk) = pick_disk("Restore onto which disk?", boot_device) else {
        return;
    };
    let (_, analysis) = match read_disk(disk.handle) {
        Ok(v) => v,
        Err(e) => return show_error("Restore GPT", e),
    };

    let restore = match backup::restore_plan(archive, &analysis) {
        Ok(p) => p,
        Err(mismatch) => {
            return show_error(
                "Restore GPT refused",
                format!("this backup does not fit this disk: {mismatch}"),
            )
        }
    };

    let mut lines = alloc::vec![format!("  {}", disk.label()), String::new()];
    lines.extend(backup::describe(archive));
    if archive.disk_guid != current_disk_guid(&analysis) {
        lines.push(String::new());
        lines.push(String::from("  NOTE: the disk GUID on this disk differs from the one"));
        lines.push(String::from("  in the backup. Geometry matches, so this is allowed,"));
        lines.push(String::from("  but check it is really the disk you mean."));
    }
    lines.extend(report::render_entries(
        &restore.entries,
        analysis.block_size,
        restore.header.disk_guid,
        "Table that will be restored:",
    ));
    lines.extend(report::render_plan(&restore));
    if !ui::page("Restore GPT", &lines) {
        return;
    }

    let mut warning = alloc::vec![
        format!("  {}", disk.label()),
        String::new(),
        String::from("  This REPLACES both partition tables with the saved copy."),
    ];
    if !archive.health.tables_were_sound() {
        warning.push(String::new());
        warning.push(String::from("  WARNING: this snapshot was taken from a DAMAGED table."));
    }
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_error("Restore GPT", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &restore);
    report_write("Restore GPT", &disk, result, esp_lost);
}

/// The disk GUID currently on the disk, for comparison against a backup.
fn current_disk_guid(analysis: &Analysis) -> gptcore::Guid {
    analysis.best_view().map(|t| t.header.disk_guid).unwrap_or(gptcore::Guid::ZERO)
}

/// Close the gap that the corrupting writer's arithmetic depends on.
fn run_prevent(boot_device: &BootDevice, esp_lost: &mut bool) {
    let Some(disk) = pick_disk("Prevent recurrence", boot_device) else {
        return;
    };
    let (_, analysis) = match read_disk(disk.handle) {
        Ok(v) => v,
        Err(e) => return show_error("Prevent", e),
    };

    let verdict = prevent::assess(&analysis);
    let mut lines = alloc::vec![format!("  {}", disk.label()), String::new()];
    lines.extend(prevent::describe(verdict));

    if !ui::page("Prevent recurrence", &lines) {
        return;
    }
    if !verdict.will_write() {
        return;
    }
    let Some(gap_plan) = prevent::plan(&analysis, &CRC) else {
        return;
    };

    let mut plan_lines = alloc::vec![format!("  {}", disk.label()), String::new()];
    plan_lines.extend(report::render_plan(&gap_plan));
    if !ui::page("Prevent recurrence: what will be written", &plan_lines) {
        return;
    }

    let warning = alloc::vec![
        format!("  {}", disk.label()),
        String::new(),
        String::from("  This modifies a HEALTHY partition table on a theory"),
        String::from("  about what corrupts it. No partition moves, and it is"),
        String::from("  reversible, but it is not a repair."),
    ];
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_error("Prevent", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &gap_plan);
    report_write("Prevent", &disk, result, esp_lost);
}

fn main_menu_items() -> Vec<ui::Item> {
    alloc::vec![
        ui::Item::with_detail(
            "Check GPT",
            alloc::vec![
                String::from("  Read both tables and report what is wrong."),
                String::from("  Writes nothing."),
            ],
        ),
        ui::Item::with_detail(
            "Repair primary GPT from the backup",
            alloc::vec![
                String::from("  Rebuild a corrupt primary table from the backup"),
                String::from("  at the end of the disk."),
            ],
        ),
        ui::Item::with_detail(
            "Back up both GPTs to the ESP",
            alloc::vec![
                String::from("  Save the tables to a file on this volume, so they"),
                String::from("  can be put back exactly as they are now."),
            ],
        ),
        ui::Item::with_detail(
            "Restore GPTs from a saved backup",
            alloc::vec![
                String::from("  Write a previously saved snapshot back onto the"),
                String::from("  disk it was taken from."),
            ],
        ),
        ui::Item::with_detail(
            "Prevent recurrence (close the FirstUsableLBA gap)",
            alloc::vec![
                String::from("  Lower FirstUsableLBA so the arithmetic that caused"),
                String::from("  the corruption produces the right answer."),
            ],
        ),
        ui::Item::with_detail("Exit", alloc::vec![String::from("  Return to the firmware.")]),
    ]
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("failed to initialise uefi helpers");
    ui::hide_cursor();

    let boot_device = BootDevice::resolve();
    let mut intro = alloc::vec![format!("  version {}", env!("CARGO_PKG_VERSION"))];
    if boot_device.is_known() {
        intro.push(format!("  launched from {}", path_text(boot_device.path())));
    } else {
        intro.push(String::from("  boot volume unknown - no disk will be marked [boot]"));
    }

    let items = main_menu_items();
    let mut esp_lost = false;

    loop {
        match ui::menu("efigptfix", &intro, &items, "B = exit") {
            Some(0) => run_check(&boot_device),
            Some(1) => run_repair(&boot_device, &mut esp_lost),
            Some(2) => run_backup(&boot_device, esp_lost),
            Some(3) => run_restore(&boot_device, &mut esp_lost),
            Some(4) => run_prevent(&boot_device, &mut esp_lost),
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
