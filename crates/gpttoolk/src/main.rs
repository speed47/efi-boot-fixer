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
mod espscan;
mod fwcrc;
mod gfx;
mod nvram;
mod selfdev;
mod ui;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use blockdev::UefiDisk;
use fwcrc::FirmwareCrc32;
use gptcore::backup::{self, Timestamp};
use gptcore::repair::{analyze, apply, plan, Analysis, RepairPlan};
use gptcore::style::{bad, dim, good, key, line, title, warn, Line, Style};
use gptcore::{bootopt, layout, prevent, report};
use selfdev::BootDevice;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol, SearchType};
use uefi::prelude::*;
use uefi::proto::device_path::text::{AllowShortcuts, DisplayOnly};
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::proto::ProtocolPointer;
use uefi::Identify;

const CRC: FirmwareCrc32 = FirmwareCrc32;

/// What the operator sees at the top of the main menu.
///
/// The hardware is named because that is what someone searching for this
/// will have typed, and because the layout checks and the prevention
/// hypothesis are both specific to it. It is a suffix so that supporting
/// another handheld with the same fault is a deletion rather than a
/// rewrite: `EFI GPT Toolkit` stands on its own. The binary is named from
/// the part that survives that deletion, so it never has to be renamed.
const APP_NAME: &str = "EFI GPT Toolkit for Steam Deck";

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
    /// The table as `scan` found it, or why it could not be read.
    ///
    /// Carried rather than re-read. Reading a GPT is four block reads and
    /// two CRCs over the entry array, and every operation used to repeat
    /// the whole thing on a disk the picker had just analysed to decide
    /// what to say about it. Restore was the worst case: it analysed every
    /// attached disk three times over before writing anything.
    ///
    /// This is a snapshot, and that is sound because a scan is never reused
    /// across operations — each one picks a disk, which rescans — and
    /// because nothing else in the firmware is touching the disk while the
    /// operator reads a menu. The one thing that does invalidate it is our
    /// own write, and no plan is built from an analysis taken after one.
    analysis: Result<Analysis, String>,
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

    fn detail(&self) -> Vec<Line> {
        let (health, style) = match &self.analysis {
            Ok(a) => {
                (report::verdict_line(a.verdict).to_string(), report::verdict_style(a.verdict))
            }
            Err(e) => (format!("could not be read - {e}"), Style::Bad),
        };
        let mut out = ui::wrapped(&format!("  {}", self.path), Style::Dim, "    ");
        out.push(dim(format!("  {} blocks x {} B", self.last_block + 1, self.block_size)));
        out.push(Line::new(format!("  GPT: {health}"), style));
        out
    }
}

fn open_disk(handle: Handle) -> Result<UefiDisk, String> {
    let io = get_protocol::<BlockIO>(handle).map_err(|e| format!("cannot open disk ({e})"))?;
    UefiDisk::new(io).map_err(|e| e.to_string())
}

fn read_disk(handle: Handle) -> Result<Analysis, String> {
    let mut disk = open_disk(handle)?;
    analyze(&mut disk, &CRC).map_err(|e| format!("cannot read the GPT ({e})"))
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

        let analysis = read_disk(handle);
        // Precomputed rather than derived on demand: `label` is called for
        // every row on every keypress, and this walks the entry array.
        let steamos = analysis.as_ref().is_ok_and(|a| {
            a.best_view().map(|t| layout::steamos_hint(&t.entries)).unwrap_or_default().likely()
        });

        disks.push(Disk {
            handle,
            number: disks.len() + 1,
            path: path_text(path.as_deref()),
            boot,
            model: diskinfo::model(handle),
            block_size,
            last_block,
            steamos,
            analysis,
        });
    }
    disks
}

/// Choose a disk, or `None` if the operator backed out.
fn pick_disk(title: &str, boot_device: &BootDevice) -> Option<Disk> {
    pick_from(title, scan(boot_device))
}

/// The same picker, over disks already scanned.
///
/// Restore needs the list before it can offer anything — it has to work out
/// which disk each snapshot belongs to — so it scans once and picks from
/// that, rather than throwing the analyses away and having `pick_disk`
/// redo every one of them.
fn pick_from(title: &str, disks: Vec<Disk>) -> Option<Disk> {
    if disks.is_empty() {
        ui::message(
            "Nothing to do",
            &alloc::vec![
                bad("  No fixed, writable, whole disks were found."),
                Line::blank(),
                dim("  Only removable media is present, and this tool"),
                dim("  does not offer to rewrite that."),
            ],
        );
        return None;
    }

    let intro = alloc::vec![
        dim("  Choose a disk. Removable media is not listed."),
        dim("  [boot] carries the volume this program came from."),
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
                good("  Written and flushed."),
                Line::blank(),
                key("  Reboot when you are done."),
            ];
            if !exclusive {
                lines.push(Line::blank());
                lines.push(warn("  (The disk was busy, so the write was made without"));
                lines.push(warn("  exclusive access. The firmware's own view of the"));
                lines.push(warn("  partitions is stale until you reboot.)"));
            }
            ui::message(title, &lines);
        }
        Err(e) => ui::message(
            &format!("{title} FAILED"),
            &alloc::vec![
                bad(format!("  {e}")),
                Line::blank(),
                bad("  Do NOT reboot into the OS; use a rescue USB."),
            ],
        ),
    }
}

fn show_error(title: &str, message: String) {
    ui::message(title, &alloc::vec![bad(format!("  {message}"))]);
}

/// Not a failure: a refusal, or a choice the operator made.
fn show_note(title: &str, message: String) {
    ui::message(title, &alloc::vec![warn(format!("  {message}"))]);
}

/// Read-only: say what is wrong, write nothing, offer nothing.
fn run_check(boot_device: &BootDevice) {
    let Some(disk) = pick_disk("Check GPT", boot_device) else {
        return;
    };
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Check GPT", e.clone()),
    };

    let mut lines = alloc::vec![key(format!("  {}", disk.label()))];
    lines.extend(ui::wrapped(&format!("  {}", disk.path), Style::Dim, "    "));
    lines.push(Line::blank());
    lines.extend(report::render_analysis(analysis));
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
    lines.push(Line::blank());
    lines.push(good("  Nothing was written. This check never modifies a disk."));
    ui::page("Check GPT (read only)", &lines);
}

/// Rebuild a corrupt primary GPT from the backup table.
fn run_repair(boot_device: &BootDevice, esp_lost: &mut bool) {
    let Some(disk) = pick_disk("Repair primary GPT", boot_device) else {
        return;
    };
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Repair", e.clone()),
    };

    let repair = plan(analysis, &CRC);
    let mut lines = alloc::vec![key(format!("  {}", disk.label())), Line::blank()];
    lines.extend(report::render(analysis, repair.as_ref()));

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
            key(format!("  {}", disk.label())),
            Line::blank(),
            warn("  This rewrites the protective MBR on this disk."),
            line("  Both GPTs are intact and are left alone."),
        ]
    } else {
        alloc::vec![
            key(format!("  {}", disk.label())),
            Line::blank(),
            bad("  This REWRITES the partition table on this disk."),
            line("  The proposed table came from the backup GPT and was"),
            line("  shown on the previous screen."),
        ]
    };
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_note("Repair", "Cancelled. Nothing was written.".to_string());
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
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Back up GPT", e.clone()),
    };
    // The only operation that also needs the device: capture copies the
    // MBR, both headers and both entry arrays out of it.
    let mut dev = match open_disk(disk.handle) {
        Ok(dev) => dev,
        Err(e) => return show_error("Back up GPT", e),
    };

    let archive = match backup::capture(&mut dev, analysis, now(), provenance(&disk, boot_device)) {
        Ok(a) => a,
        Err(e) => return show_error("Back up GPT", format!("could not read the tables ({e})")),
    };
    let bytes = backup::encode(&archive, &CRC);

    let mut lines = alloc::vec![key(format!("  {}", disk.label())), Line::blank()];
    lines.extend(backup::describe(&archive));
    lines.push(Line::blank());
    lines.push(line(format!(
        "  {} bytes will be written to \\{}\\ on the ESP",
        bytes.len(),
        esp::DIR
    )));
    if disk.boot {
        lines.push(Line::blank());
        lines.push(warn("  NOTE: the ESP is on this same disk, so this is a"));
        lines.push(warn("  convenience copy, not an off-device backup. Copy the"));
        lines.push(warn("  file elsewhere once you can boot again."));
    }
    if !ui::page("Back up GPT", &lines) {
        return;
    }

    // Writing a file needs no confirmation sequence: it creates data, it
    // does not overwrite a partition table, and it never replaces an
    // existing snapshot.
    let name = match next_name() {
        Ok(n) => n,
        Err(e) => return show_error("Back up GPT FAILED", e),
    };
    match esp::save(&name, &bytes) {
        Ok(path) => ui::message(
            "Back up GPT",
            &alloc::vec![
                good("  Saved to the ESP as:"),
                key(format!("    {path}")),
                Line::blank(),
                dim("  Restore reads it from there."),
            ],
        ),
        Err(e) => show_error("Back up GPT FAILED", e),
    }
}

/// The next free snapshot name on the ESP.
///
/// A listing that failed is propagated, never treated as an empty ESP: the
/// name is chosen by counting from the highest sequence present, so a
/// snapshot this does not see is a snapshot the new name could collide with.
fn next_name() -> Result<String, String> {
    let taken: Vec<String> = esp::list()?.into_iter().map(|s| s.name).collect();
    backup::next_name(&taken).ok_or_else(|| {
        format!(
            "\\{}\\ already holds gpt.{}; delete some snapshots first",
            esp::DIR,
            backup::MAX_SEQUENCE
        )
    })
}

/// What was true when a snapshot was taken, for whoever reads it later.
///
/// None of this is needed to restore the file. It is here for the question
/// asked years afterwards: is this from this machine, and what was it?
fn provenance(disk: &Disk, boot_device: &BootDevice) -> Vec<(String, String)> {
    let mut meta = alloc::vec![(
        String::from("tool"),
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("GPTTOOLK_VERSION"))
    )];

    let vendor = uefi::system::firmware_vendor().to_string();
    if !vendor.is_empty() {
        meta.push((
            String::from("firmware"),
            format!("{} rev {:#x}", vendor, uefi::system::firmware_revision()),
        ));
    }
    let rev = uefi::system::uefi_revision();
    meta.push((String::from("uefi"), format!("{}.{}", rev.major(), rev.minor())));

    meta.push((String::from("device"), disk.path.clone()));
    if let Some(model) = &disk.model {
        meta.push((String::from("model"), model.clone()));
    }
    meta.push((
        String::from("capacity"),
        format!("{} blocks x {} B", disk.last_block + 1, disk.block_size),
    ));
    if boot_device.is_known() {
        meta.push((String::from("launched-from"), path_text(boot_device.path())));
    }
    meta
}

fn warn_esp_may_be_gone() -> bool {
    ui::page(
        "The ESP may no longer be reachable",
        &alloc::vec![
            warn("  A disk was written to with exclusive access during this"),
            warn("  session, which disconnects the firmware's filesystem"),
            warn("  driver for that disk."),
            Line::blank(),
            line("  Reading or writing files on the ESP may fail until you"),
            key("  reboot. Continue anyway?"),
        ],
    )
}

/// A snapshot found on the ESP, with whatever we can work out about where
/// it came from.
struct Saved {
    name: String,
    archive: backup::Archive,
    /// The disk this most likely belongs to, and why we think so.
    best: Option<(usize, backup::Comparison)>,
}

impl Saved {
    fn label(&self) -> String {
        format!("{:<8} {}", self.name, backup::summary(&self.archive))
    }

    fn belongs_to(&self, disks: &[Disk]) -> Line {
        match &self.best {
            Some((i, c)) => Line::new(
                format!("  Belongs to: Disk {} - {}", disks[*i].number, c.describe()),
                c.style(),
            ),
            None => bad(String::from("  Belongs to: no disk here that it fits")),
        }
    }
}

/// Work out which of the disks present each snapshot belongs to.
///
/// Geometry alone would be a weak answer on a machine with two identical
/// drives, so this leans on the per-partition unique GUIDs; see
/// `backup::Comparison`.
fn attribute(archives: Vec<(String, backup::Archive)>, disks: &[Disk]) -> Vec<Saved> {
    archives
        .into_iter()
        .map(|(name, archive)| {
            let mut best: Option<(usize, backup::Comparison)> = None;
            for (i, disk) in disks.iter().enumerate() {
                let Ok(analysis) = &disk.analysis else { continue };
                let c = backup::compare(&archive, analysis);
                if c.verdict() == backup::Match::DifferentDisk {
                    continue;
                }
                let better = match &best {
                    None => true,
                    Some((_, prev)) => {
                        (c.verdict() == backup::Match::SameDisk
                            && prev.verdict() != backup::Match::SameDisk)
                            || c.shared_partitions > prev.shared_partitions
                    }
                };
                if better {
                    best = Some((i, c));
                }
            }
            Saved { name, archive, best }
        })
        .collect()
}

/// What was found in `\GPTTOOLK`: the snapshots that decoded, and a
/// readable complaint about each one that did not.
struct Found {
    usable: Vec<(String, backup::Archive)>,
    rejected: Vec<Line>,
}

/// Read and decode every snapshot on the ESP, reporting the ones that
/// cannot be used rather than quietly dropping them.
fn load_saved() -> Result<Found, String> {
    let mut found = Found { usable: Vec::new(), rejected: Vec::new() };
    for file in esp::list()? {
        let data = match file.data {
            Ok(data) => data,
            Err(e) => {
                found.rejected.push(bad(format!("  {} - {e}", file.name)));
                continue;
            }
        };
        match backup::decode(&data, &CRC) {
            Ok(a) => found.usable.push((file.name, a)),
            Err(e) => found.rejected.push(bad(format!("  {} - {e}", file.name))),
        }
    }
    Ok(found)
}

/// Put a saved snapshot back.
fn run_restore(boot_device: &BootDevice, esp_lost: &mut bool) {
    if *esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    let Found { usable, rejected } = match load_saved() {
        Ok(v) => v,
        Err(e) => return show_error("Restore GPT", e),
    };

    if usable.is_empty() {
        let mut lines =
            alloc::vec![warn(format!("  No usable snapshots in \\{}\\ on the ESP.", esp::DIR))];
        if !rejected.is_empty() {
            lines.push(Line::blank());
            lines.push(warn("  Rejected:"));
            lines.extend(rejected);
        }
        return ui::message("Restore GPT", &lines);
    }
    if !rejected.is_empty() {
        let mut lines = alloc::vec![
            warn("  Some files could not be read and are not offered"),
            warn("  below:"),
            Line::blank(),
        ];
        lines.extend(rejected);
        ui::message("Restore GPT", &lines);
    }

    let disks = scan(boot_device);
    let saved = attribute(usable, &disks);

    let intro = alloc::vec![
        dim(format!("  {} snapshot(s) in \\{}\\ on the ESP.", saved.len(), esp::DIR)),
        dim("  A snapshot only fits the disk it was taken from."),
    ];
    let items: Vec<ui::Item> = saved
        .iter()
        .map(|sv| {
            ui::Item::with_detail(
                sv.label(),
                alloc::vec![
                    sv.belongs_to(&disks),
                    dim(format!("  disk GUID {}", sv.archive.disk_guid)),
                    dim(match sv.archive.meta_get("tool") {
                        Some(t) => format!("  written by {t}"),
                        None => String::from("  written by an older build"),
                    }),
                ],
            )
        })
        .collect();

    // Browse, inspect, choose. Inspecting returns to the same row.
    let mut selected = 0usize;
    let archive = loop {
        match ui::menu_inspectable(
            "Restore GPT",
            &intro,
            &items,
            "B = back",
            "View = details",
            selected,
        ) {
            ui::Choice::Cancelled => return,
            ui::Choice::Item(i) => break &saved[i].archive,
            ui::Choice::Inspect(i) => {
                selected = i;
                let sv = &saved[i];
                let mut lines = alloc::vec![key(format!("  {}", sv.name)), Line::blank()];
                let against =
                    sv.best.as_ref().map(|(d, c)| (format!("Disk {}", disks[*d].number), c));
                lines.extend(backup::inspect(
                    &sv.archive,
                    against.as_ref().map(|(name, c)| (name.as_str(), *c)),
                ));
                ui::page(&format!("Snapshot {}", sv.name), &lines);
            }
        }
    };

    // `disks` is the scan attribution already did; picking from it saves
    // analysing every attached disk a second time.
    let Some(disk) = pick_from("Restore onto which disk?", disks) else {
        return;
    };
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Restore GPT", e.clone()),
    };

    let restore = match backup::restore_plan(archive, analysis) {
        Ok(p) => p,
        Err(mismatch) => {
            return show_error(
                "Restore GPT refused",
                format!("this snapshot does not fit this disk: {mismatch}"),
            )
        }
    };

    let comparison = backup::compare(archive, analysis);
    let mut lines = alloc::vec![key(format!("  {}", disk.label())), Line::blank()];
    let disk_name = format!("Disk {}", disk.number);
    lines.extend(backup::inspect(archive, Some((disk_name.as_str(), &comparison))));
    if comparison.verdict() == backup::Match::SameGeometry {
        lines.push(Line::blank());
        lines.push(warn("  NOTE: nothing in this snapshot identifies it as this"));
        lines.push(warn("  disk's own. The geometry fits, so it can be written,"));
        lines.push(warn("  but check it is really the disk you mean."));
    }
    lines.extend(report::render_plan(&restore));
    if !ui::page("Restore GPT", &lines) {
        return;
    }

    let mut warning = alloc::vec![
        key(format!("  {}", disk.label())),
        Line::blank(),
        bad("  This REPLACES both partition tables with the saved copy."),
    ];
    if !archive.health.tables_were_sound() {
        warning.push(Line::blank());
        warning.push(bad("  WARNING: this snapshot was taken from a DAMAGED table."));
    }
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_note("Restore GPT", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &restore);
    report_write("Restore GPT", &disk, result, esp_lost);
}

/// Close the gap that the corrupting writer's arithmetic depends on.
fn run_prevent(boot_device: &BootDevice, esp_lost: &mut bool) {
    let Some(disk) = pick_disk("Prevent recurrence", boot_device) else {
        return;
    };
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Prevent", e.clone()),
    };

    let verdict = prevent::assess(analysis);
    let mut lines = alloc::vec![key(format!("  {}", disk.label())), Line::blank()];
    lines.extend(prevent::describe(verdict));

    if !ui::page("Prevent recurrence", &lines) {
        return;
    }
    if !verdict.will_write() {
        return;
    }
    let Some(gap_plan) = prevent::plan(analysis, &CRC) else {
        return;
    };

    let mut plan_lines = alloc::vec![key(format!("  {}", disk.label())), Line::blank()];
    plan_lines.extend(report::render_plan(&gap_plan));
    if !ui::page("Prevent recurrence: what will be written", &plan_lines) {
        return;
    }

    let warning = alloc::vec![
        key(format!("  {}", disk.label())),
        Line::blank(),
        warn("  This modifies a HEALTHY partition table on a theory"),
        warn("  about what corrupts it. No partition moves, and it is"),
        warn("  reversible, but it is not a repair."),
    ];
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_note("Prevent", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &gap_plan);
    report_write("Prevent", &disk, result, esp_lost);
}

// --------------------------------------------------- NVRAM boot entries

/// Render the device path held in a boot entry.
///
/// Distinguishes "there is no path" from "there is one and it will not
/// parse". The second is a finding — a truncated variable — and must not be
/// shown as a blank line.
fn boot_path_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match <&DevicePath>::try_from(bytes) {
        Ok(path) => path_text(Some(path)),
        Err(_) => format!("<{} bytes that are not a device path>", bytes.len()),
    }
}

/// One entry, as it appears on the boot entries page.
fn render_boot_entry(slot: u16, entry: &Result<bootopt::LoadOption, String>) -> Vec<Line> {
    let opt = match entry {
        Ok(opt) => opt,
        Err(e) => {
            return alloc::vec![bad(format!(
                "  {}  cannot be read - {e}",
                bootopt::slot_name(slot)
            ))]
        }
    };
    let mut out = alloc::vec![Line::new(format!("  {}", bootopt::summary(slot, opt)), opt.style())];
    let path = boot_path_text(&opt.device_path);
    if !path.is_empty() {
        out.extend(ui::wrapped(&format!("      {path}"), Style::Dim, "        "));
    }
    for line in bootopt::render_flags(opt) {
        out.push(Line::new(format!("      {}", line.text.trim_start()), line.style));
    }
    out
}

fn slot_with_name(state: &nvram::BootState, slot: u16) -> String {
    match state.get(slot) {
        Some(Ok(opt)) => format!("{}  {}", bootopt::slot_name(slot), opt.description),
        Some(Err(_)) => format!("{}  (unreadable)", bootopt::slot_name(slot)),
        None => format!("{}  (no such entry)", bootopt::slot_name(slot)),
    }
}

/// Everything NVRAM says about booting. Reads nothing else and writes
/// nothing at all.
fn run_boot_view() {
    let state = nvram::read();
    let mut lines = Vec::new();

    if let Some(e) = &state.truncated {
        lines.push(bad(format!("  {e}")));
        lines.push(warn("  The list below may be incomplete."));
        lines.push(Line::blank());
    }

    lines.push(key(format!(
        "  Booted from: {}",
        match state.current {
            Some(s) => slot_with_name(&state, s),
            None => String::from("the firmware did not say"),
        }
    )));
    if let Some(next) = state.next {
        lines.push(warn(format!("  Next boot:   {} (one shot)", slot_with_name(&state, next))));
    }
    if let Some(t) = state.timeout {
        lines.push(dim(format!("  Menu timeout: {t} s")));
    }
    lines.push(Line::blank());

    match &state.order {
        Err(e) => {
            lines.push(bad(format!("  BootOrder cannot be read - {e}")));
        }
        Ok(order) if order.is_empty() => {
            lines.push(bad("  BootOrder is empty."));
            lines.push(line("  The firmware has no list to try, and will fall back"));
            lines.push(line("  to \\EFI\\BOOT\\BOOTX64.EFI if it is willing to."));
        }
        Ok(order) => {
            lines.push(title(format!("  Boot order ({} entries):", order.len())));
            for (i, slot) in order.iter().enumerate() {
                match state.get(*slot) {
                    Some(entry) => {
                        let mut rendered = render_boot_entry(*slot, entry);
                        // Number the position, so "third in the list" is
                        // readable off the screen rather than counted.
                        rendered[0].text = format!("  {:>2}. {}", i + 1, &rendered[0].text[2..]);
                        lines.extend(rendered);
                    }
                    None => lines.push(bad(format!(
                        "  {:>2}. {}  <- in the order, but no such entry",
                        i + 1,
                        bootopt::slot_name(*slot)
                    ))),
                }
            }
        }
    }

    // The reason this screen enumerates the store instead of walking
    // BootOrder. An entry here is installed, bootable, and invisible to the
    // firmware's own menu.
    let orphans = state.orphans();
    if !orphans.is_empty() {
        lines.push(Line::blank());
        lines.push(title("  Present, but not in the boot order:"));
        lines.push(dim("  The firmware will not offer these."));
        for slot in orphans {
            if let Some(entry) = state.get(slot) {
                lines.extend(render_boot_entry(slot, entry));
            }
        }
    }

    if state.entries.is_empty() {
        lines.push(Line::blank());
        lines.push(bad("  There are no boot entries in NVRAM at all."));
    }

    lines.push(Line::blank());
    lines.push(good("  Nothing was written. This screen never modifies NVRAM."));
    ui::page("Boot entries in NVRAM (read only)", &lines);
}

/// What is actually installed on the ESPs, and whether NVRAM knows about it.
fn run_boot_scan(boot_device: &BootDevice) {
    let state = nvram::read();
    let known: Vec<(u16, Vec<u8>)> = state
        .entries
        .iter()
        .filter_map(|(slot, e)| e.as_ref().ok().map(|o| (*slot, o.device_path.clone())))
        .collect();
    let scan = espscan::scan(boot_device, &known);

    let mut lines = Vec::new();
    if scan.volumes.is_empty() {
        lines.push(bad("  No EFI System Partition was found on a fixed disk."));
        lines.push(Line::blank());
        lines.push(dim("  Removable media is not scanned: a boot entry pointing"));
        lines.push(dim("  at a USB stick stops working when it is unplugged."));
        ui::page("Bootloaders on the ESPs", &lines);
        return;
    }

    for (i, volume) in scan.volumes.iter().enumerate() {
        if i > 0 {
            lines.push(Line::blank());
        }
        let mut heading = format!("  ESP {}", i + 1);
        if volume.boot {
            heading.push_str("  [boot]");
        }
        lines.push(key(heading));
        lines.extend(ui::wrapped(&format!("    {}", volume.path), Style::Dim, "      "));
        lines.push(dim(format!("    partition {}", volume.partition)));

        if scan.unreadable.contains(&i) {
            lines.push(bad("    The filesystem on this ESP could not be opened."));
            continue;
        }

        let found: Vec<&espscan::Candidate> =
            scan.candidates.iter().filter(|c| c.esp == i).collect();
        if found.is_empty() {
            lines.push(warn("    No EFI binaries on this ESP."));
            continue;
        }
        for c in found {
            lines.extend(ui::wrapped(&format!("    {}", c.file), Style::Normal, "      "));
            lines.push(dim(format!("      {}, {}", c.kind, report::human_size(c.size))));
            match c.registered {
                Some(slot) => {
                    lines.push(good(format!("      registered as {}", bootopt::slot_name(slot))))
                }
                None => lines.push(warn(String::from("      not in NVRAM"))),
            }
        }
    }

    let unregistered = scan.candidates.iter().filter(|c| c.registered.is_none()).count();
    lines.push(Line::blank());
    if unregistered > 0 {
        lines.push(line(format!(
            "  {unregistered} of {} have no boot entry pointing at them.",
            scan.candidates.len()
        )));
        lines.push(dim("  Registering them is not offered yet."));
    }
    lines.push(good("  Nothing was written. This screen never modifies NVRAM."));
    ui::page("Bootloaders on the ESPs (read only)", &lines);
}

// ------------------------------------------------- NVRAM writes (phase 2)

/// Save the whole boot configuration to the ESP.
fn take_boot_snapshot() -> Result<String, String> {
    let (vars, missed) = nvram::capture();
    if vars.is_empty() {
        return Err(String::from("there is nothing in NVRAM to save"));
    }
    let mut meta = alloc::vec![(
        String::from("tool"),
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("GPTTOOLK_VERSION"))
    )];
    let vendor = uefi::system::firmware_vendor().to_string();
    if !vendor.is_empty() {
        meta.push((
            String::from("firmware"),
            format!("{} rev {:#x}", vendor, uefi::system::firmware_revision()),
        ));
    }
    // A partial copy that says so is worth having; one that pretends to be
    // complete is not.
    if !missed.is_empty() {
        meta.push((String::from("unread"), missed.join(" ")));
    }

    let snap = gptcore::bootcfg::Snapshot { time: now(), vars, meta };
    let bytes = gptcore::bootcfg::encode(&snap, &CRC);
    let name = gptcore::bootcfg::next_name(&esp::names()?).ok_or_else(|| {
        format!(
            "\\{}\\ already holds boot.{}; delete some first",
            esp::DIR,
            gptcore::bootcfg::MAX_SEQUENCE
        )
    })?;
    esp::save(&name, &bytes)
}

/// Save the boot configuration before this session's first NVRAM write.
///
/// Mandatory, but not a hard refusal when it fails. On a machine whose ESP
/// has gone unreachable, changing a boot entry may be the only remedy
/// left, and refusing outright would make the tool useless exactly when it
/// is needed. So a failure becomes a question with the consequence spelled
/// out, and `taken` stays false so the next write tries again.
fn ensure_boot_snapshot(taken: &mut bool, esp_lost: bool) -> bool {
    if *taken {
        return true;
    }
    if esp_lost && !warn_esp_may_be_gone() {
        return false;
    }
    match take_boot_snapshot() {
        Ok(path) => {
            *taken = true;
            ui::message(
                "Boot configuration saved",
                &alloc::vec![
                    good("  The boot configuration as it is now was saved to:"),
                    key(format!("    {path}")),
                    Line::blank(),
                    dim("  Taken automatically before the first change of"),
                    dim("  this session, so there is a record to go back to."),
                ],
            );
            true
        }
        Err(e) => ui::page(
            "Could not save the boot configuration",
            &alloc::vec![
                bad(format!("  {e}")),
                Line::blank(),
                warn("  Nothing has been changed yet. Continuing means"),
                warn("  making the change with no saved copy of what the"),
                warn("  boot configuration looks like right now."),
                Line::blank(),
                key("  Continue without a saved copy?"),
            ],
        ),
    }
}

/// Show the plan, take the snapshot, take the confirmation, write.
///
/// One funnel for all three operations so the order of those four steps is
/// decided once. The snapshot comes after the review page and before the
/// gate: no file is written for a plan the operator walked away from, and
/// the gate stays the last thing between them and NVRAM.
fn authorise_and_write(
    title: &str,
    review: Vec<Line>,
    warning: Vec<Line>,
    writes: &[bootopt::VarWrite],
    snapshot: &mut bool,
    esp_lost: bool,
) {
    let mut lines = review;
    lines.push(Line::blank());
    lines.extend(bootopt::render_plan(writes));
    if !ui::page(title, &lines) {
        return;
    }
    if !ensure_boot_snapshot(snapshot, esp_lost) {
        return show_note(title, "Cancelled. Nothing was written.".to_string());
    }
    if !ui::confirm_sequence("Authorise NVRAM write", &warning) {
        return show_note(title, "Cancelled. Nothing was written.".to_string());
    }

    match nvram::apply(writes) {
        Ok(()) => ui::message(
            title,
            &alloc::vec![
                good("  Written to NVRAM."),
                Line::blank(),
                key("  Reboot for this to take effect."),
            ],
        ),
        Err((done, e)) => {
            let mut lines = alloc::vec![bad(format!("  {e}")), Line::blank()];
            if done == 0 {
                lines.push(good("  Nothing was written; NVRAM is as it was."));
            } else {
                lines
                    .push(warn(format!("  {done} of {} writes had already landed:", writes.len())));
                for w in writes.iter().take(done) {
                    lines.push(key(format!("    {}", w.name)));
                }
                lines.push(Line::blank());
                lines.push(line("  \"View the boot entries\" shows the result. An"));
                lines.push(line("  entry that is not in the boot order is harmless;"));
                lines.push(line("  the firmware ignores it."));
            }
            ui::message(&format!("{title} FAILED"), &lines);
        }
    }
}

/// What to call a newly registered loader.
///
/// Generated rather than typed: there is no keyboard. The kind is what a
/// person would call it, and an unrecognised binary gets its filename,
/// which is the only true thing available.
fn suggested_description(c: &espscan::Candidate, esps: usize) -> String {
    let file = c.file.rsplit('\\').next().unwrap_or(&c.file);
    let mut name =
        if c.kind.starts_with("unrecognised") { String::from(file) } else { String::from(c.kind) };
    // Two disks can hold the same loader; without this they would be two
    // entries reading identically in the firmware's menu.
    if esps > 1 {
        name.push_str(&format!(" (ESP {})", c.esp + 1));
    }
    name
}

/// Choose one of the entries in NVRAM.
fn pick_boot_entry(title: &str, intro: &[Line], state: &nvram::BootState) -> Option<u16> {
    let order = state.order.as_deref().unwrap_or(&[]);
    // Boot order first, then the orphans: the list reads the way the
    // firmware will act, and making an orphan the default is a real repair.
    let mut slots: Vec<u16> = order.iter().copied().filter(|s| state.get(*s).is_some()).collect();
    slots.extend(state.orphans());

    let items: Vec<ui::Item> = slots
        .iter()
        .map(|slot| {
            let (label, detail) = match state.get(*slot) {
                Some(Ok(opt)) => (
                    bootopt::summary(*slot, opt),
                    alloc::vec![dim(format!("  {}", boot_path_text(&opt.device_path)))],
                ),
                _ => (
                    format!("{}  (cannot be read)", bootopt::slot_name(*slot)),
                    alloc::vec![bad("  This entry will not decode.")],
                ),
            };
            let mut detail = detail;
            if !order.contains(slot) {
                detail.push(warn("  Not in the boot order."));
            }
            ui::Item::with_detail(label, detail)
        })
        .collect();

    if items.is_empty() {
        ui::message(title, &alloc::vec![warn("  There are no boot entries in NVRAM.")]);
        return None;
    }
    ui::menu(title, intro, &items, "B = back").map(|i| slots[i])
}

/// Put a loader found on an ESP into NVRAM.
fn run_boot_register(boot_device: &BootDevice, snapshot: &mut bool, esp_lost: bool) {
    let state = nvram::read();
    let known: Vec<(u16, Vec<u8>)> = state
        .entries
        .iter()
        .filter_map(|(slot, e)| e.as_ref().ok().map(|o| (*slot, o.device_path.clone())))
        .collect();
    let scan = espscan::scan(boot_device, &known);

    let new: Vec<&espscan::Candidate> =
        scan.candidates.iter().filter(|c| c.registered.is_none()).collect();
    if new.is_empty() {
        return ui::message(
            "Register a bootloader",
            &alloc::vec![
                good("  Every loader found on the ESPs already has a boot"),
                good("  entry pointing at it."),
                Line::blank(),
                dim("  \"Scan the ESPs\" lists them and says which is which."),
            ],
        );
    }

    let items: Vec<ui::Item> = new
        .iter()
        .map(|c| {
            ui::Item::with_detail(
                format!("{}  {}", c.kind, c.file),
                alloc::vec![
                    dim(format!("  on ESP {}, {}", c.esp + 1, report::human_size(c.size))),
                    dim(format!(
                        "  will be called \"{}\"",
                        suggested_description(c, scan.volumes.len())
                    )),
                ],
            )
        })
        .collect();
    let intro = alloc::vec![
        dim("  Loaders on the ESPs that nothing in NVRAM points at."),
        dim("  Registering one adds a boot entry for it."),
    ];
    let Some(chosen) = ui::menu("Register a bootloader", &intro, &items, "B = back") else {
        return;
    };
    let candidate = new[chosen];

    let where_items = alloc::vec![
        ui::Item::with_detail(
            "Add it, and make it the default",
            alloc::vec![dim("  Goes to the front of the boot order.")],
        ),
        ui::Item::with_detail(
            "Add it at the end of the boot order",
            alloc::vec![
                dim("  Tried only if everything before it fails."),
                dim("  The safer choice if you are unsure."),
            ],
        ),
    ];
    let Some(placement) = ui::menu("Where in the boot order?", &[], &where_items, "B = back")
    else {
        return;
    };
    let first = placement == 0;

    let volume = &scan.volumes[candidate.esp];
    let device_path = match espscan::boot_path(volume.handle, &candidate.file) {
        Ok(p) => p,
        Err(e) => return show_error("Register a bootloader", e),
    };
    let taken: Vec<u16> = state.entries.iter().map(|(s, _)| *s).collect();
    let Some(slot) = bootopt::next_free_slot(&taken) else {
        return show_error(
            "Register a bootloader",
            String::from("every boot slot from Boot0000 to BootFFFF is taken"),
        );
    };

    let opt = bootopt::LoadOption {
        attributes: bootopt::LOAD_OPTION_ACTIVE,
        description: suggested_description(candidate, scan.volumes.len()),
        device_path,
        optional_data: Vec::new(),
    };
    let writes = bootopt::plan_register(slot, &opt, state.order.as_deref().unwrap_or(&[]), first);

    let mut review = alloc::vec![
        key(format!("  {}  as  {}", bootopt::slot_name(slot), opt.description)),
        Line::blank(),
    ];
    review.extend(ui::wrapped(
        &format!("  {}", boot_path_text(&opt.device_path)),
        Style::Dim,
        "    ",
    ));
    let warning = alloc::vec![
        key(format!("  {}  {}", bootopt::slot_name(slot), opt.description)),
        Line::blank(),
        warn("  This adds a boot entry to the firmware's NVRAM."),
        line("  No disk is written to, and the entry can be removed"),
        line("  from the firmware's own boot menu afterwards."),
    ];
    authorise_and_write("Register a bootloader", review, warning, &writes, snapshot, esp_lost);
}

/// Move an entry to the front of the boot order.
fn run_boot_default(snapshot: &mut bool, esp_lost: bool) {
    let state = nvram::read();
    let order = match &state.order {
        Ok(o) => o.clone(),
        Err(e) => return show_error("Set the default", format!("BootOrder cannot be read - {e}")),
    };

    let intro = alloc::vec![
        dim("  The firmware tries the boot order from the top."),
        dim("  The one you choose is moved to the front."),
    ];
    let Some(slot) = pick_boot_entry("Set the default boot entry", &intro, &state) else {
        return;
    };
    if order.first() == Some(&slot) {
        return show_note(
            "Set the default",
            format!("{} is already first in the boot order.", bootopt::slot_name(slot)),
        );
    }

    let after = bootopt::reorder(slot, &order);
    let name = |s: u16| match state.get(s) {
        Some(Ok(opt)) => format!("{}  {}", bootopt::slot_name(s), opt.description),
        Some(Err(_)) => format!("{}  (unreadable)", bootopt::slot_name(s)),
        None => format!("{}  (no such entry)", bootopt::slot_name(s)),
    };
    let review = bootopt::render_order_change(&order, &after, name);
    let writes = bootopt::plan_set_default(slot, &order);

    let warning = alloc::vec![
        key(format!("  {}", name(slot))),
        Line::blank(),
        warn("  This becomes what the machine boots by default."),
        line("  Only the order is changed; no entry is rewritten."),
        line("  To try it once without committing, use \"Boot"),
        line("  something once\" instead."),
    ];
    authorise_and_write("Set the default", review, warning, &writes, snapshot, esp_lost);
}

/// Set `BootNext`: one boot, then back to normal.
fn run_boot_once(snapshot: &mut bool, esp_lost: bool) {
    let state = nvram::read();
    let intro = alloc::vec![
        dim("  The next boot only. The firmware clears this as it"),
        dim("  uses it, so the boot after that is normal again."),
    ];
    let Some(slot) = pick_boot_entry("Boot something once", &intro, &state) else {
        return;
    };

    let label = match state.get(slot) {
        Some(Ok(opt)) => format!("{}  {}", bootopt::slot_name(slot), opt.description),
        _ => bootopt::slot_name(slot),
    };
    let writes = bootopt::plan_boot_next(slot);
    let review = alloc::vec![
        key(format!("  {label}")),
        Line::blank(),
        line("  The next boot will use this entry. Nothing else"),
        line("  changes, and the firmware removes the override as"),
        line("  soon as it has used it."),
        Line::blank(),
        good("  This is the safe way to test an entry: if it does"),
        good("  not work, the boot after it is the old default."),
    ];
    let warning = alloc::vec![
        key(format!("  {label}")),
        Line::blank(),
        warn("  This sets the next boot only. It reverts itself."),
    ];
    authorise_and_write("Boot something once", review, warning, &writes, snapshot, esp_lost);
}

fn run_boot_entries(boot_device: &BootDevice, esp_lost: bool) {
    let items = alloc::vec![
        ui::Item::with_detail(
            "View the boot entries in NVRAM",
            alloc::vec![
                dim("  What the firmware will try, in order, plus any"),
                dim("  entry that has fallen out of the list."),
            ],
        ),
        ui::Item::with_detail(
            "Scan the ESPs for bootloaders",
            alloc::vec![
                dim("  Which loaders are installed on disk, and whether"),
                dim("  NVRAM has an entry pointing at each one."),
            ],
        ),
        ui::Item::with_detail(
            "Register a bootloader in NVRAM",
            alloc::vec![
                dim("  Add a boot entry for a loader that is on an ESP"),
                dim("  but that nothing in NVRAM points at."),
            ],
        ),
        ui::Item::with_detail(
            "Set the default boot entry",
            alloc::vec![
                dim("  Move an entry to the front of the boot order."),
                dim("  Changes the order only; no entry is rewritten."),
            ],
        ),
        ui::Item::with_detail(
            "Boot something once (next boot only)",
            alloc::vec![
                dim("  Try an entry without committing to it. The"),
                dim("  firmware clears this as it uses it."),
            ],
        ),
    ];

    let intro = alloc::vec![
        dim("  The first two screens read only."),
        dim("  The rest change NVRAM, and say so before they do."),
    ];
    // Saved once per session, before the first change, by whichever
    // operation gets there first.
    let mut snapshot = false;
    loop {
        match ui::menu("Boot entries (NVRAM)", &intro, &items, "B = back") {
            Some(0) => run_boot_view(),
            Some(1) => run_boot_scan(boot_device),
            Some(2) => run_boot_register(boot_device, &mut snapshot, esp_lost),
            Some(3) => run_boot_default(&mut snapshot, esp_lost),
            Some(4) => run_boot_once(&mut snapshot, esp_lost),
            _ => return,
        }
    }
}

/// Restart the machine, after one confirmation.
///
/// A cold reset rather than a warm one. What is usually wanted here is for
/// the firmware to look at a disk or a variable store that this session has
/// just changed, and a warm reset is permitted to skip the initialisation
/// that re-reads them. Nothing is written on the way out, so this is gated
/// by an acknowledgement and not by the confirmation sequence — that is for
/// writes.
fn run_reboot() {
    let lines = alloc::vec![
        warn("  The machine restarts now."),
        Line::blank(),
        line("  Nothing is written by this. Anything this session"),
        line("  changed has already been written and flushed."),
    ];
    if !ui::page("Reboot", &lines) {
        return;
    }
    // The reset does not return, so this is the last chance to leave the
    // console the way the firmware handed it over.
    ui::finish();
    uefi::runtime::reset(uefi::runtime::ResetType::COLD, Status::SUCCESS, None);
}

fn main_menu_items() -> Vec<ui::Item> {
    alloc::vec![
        ui::Item::with_detail(
            "Check GPT",
            alloc::vec![
                dim("  Read both tables and report what is wrong."),
                dim("  Writes nothing.")
            ],
        ),
        ui::Item::with_detail(
            "Repair primary GPT from the backup",
            alloc::vec![
                dim("  Rebuild a corrupt primary table from the backup"),
                dim("  at the end of the disk.")
            ],
        ),
        ui::Item::with_detail(
            "Back up both GPTs to the ESP",
            alloc::vec![
                dim("  Save the tables to a file on this volume, so they"),
                dim("  can be put back exactly as they are now.")
            ],
        ),
        ui::Item::with_detail(
            "Restore GPTs from a saved backup",
            alloc::vec![
                dim("  Write a previously saved snapshot back onto the"),
                dim("  disk it was taken from.")
            ],
        ),
        ui::Item::with_detail(
            "Prevent recurrence (close the FirstUsableLBA gap)",
            alloc::vec![
                dim("  Lower FirstUsableLBA so the arithmetic that caused"),
                dim("  the corruption produces the right answer.")
            ],
        ),
        ui::Item::with_detail(
            "Boot entries (NVRAM)",
            alloc::vec![
                dim("  View the firmware's boot list, and find the"),
                dim("  bootloaders installed on the ESPs.")
            ],
        ),
        ui::Item::with_detail("Exit", alloc::vec![dim("  Return to the firmware.")]),
        ui::Item::with_detail(
            "Reboot",
            alloc::vec![
                dim("  Restart the machine, so the firmware reads the"),
                dim("  disks and the boot list again.")
            ],
        ),
    ]
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().expect("failed to initialise uefi helpers");
    // The firmware arms a five-minute watchdog before handing control to a boot
    // option, and resets the machine when it fires. A menu waiting on a keypress
    // outlives that, so the reset lands mid-session and the machine comes back
    // up on the default entry. A timeout of zero disarms it; codes below 0x10000
    // belong to the firmware, so the code here is one of ours.
    let _ = boot::set_watchdog_timer(0, 0x1_0000, None);
    // Settles which screen the menus are drawn on, and which way up, before
    // anything is drawn on it.
    ui::init();

    let boot_device = BootDevice::resolve();
    let items = main_menu_items();
    let mut esp_lost = false;

    loop {
        // Rebuilt every time round rather than once: the device path is
        // wrapped to the screen width, and the display screen can change
        // that width from any submenu this returns from. Not from this
        // menu's own display screen, though: the menu loop handles View
        // itself and never hands control back here, so a width changed
        // there leaves these lines truncated until a submenu is entered
        // and left again.
        let mut intro = alloc::vec![dim(format!("  version {}", env!("GPTTOOLK_VERSION")))];
        if boot_device.is_known() {
            intro.extend(ui::wrapped(
                &format!("  launched from {}", path_text(boot_device.path())),
                Style::Dim,
                "    ",
            ));
        } else {
            intro.push(warn("  boot volume unknown - no disk will be marked [boot]"));
        }

        match ui::menu(APP_NAME, &intro, &items, "B = exit") {
            Some(0) => run_check(&boot_device),
            Some(1) => run_repair(&boot_device, &mut esp_lost),
            Some(2) => run_backup(&boot_device, esp_lost),
            Some(3) => run_restore(&boot_device, &mut esp_lost),
            Some(4) => run_prevent(&boot_device, &mut esp_lost),
            Some(5) => run_boot_entries(&boot_device, esp_lost),
            // 6 is Exit; 7 only comes back if the reboot was not confirmed.
            Some(7) => run_reboot(),
            _ => break,
        }
    }

    if fwcrc::used_fallback() {
        ui::message(
            "Note",
            &alloc::vec![
                warn("  Firmware CalculateCrc32 was unavailable;"),
                warn("  the built-in implementation was used instead.")
            ],
        );
    }
    ui::finish();
    Status::SUCCESS
}
