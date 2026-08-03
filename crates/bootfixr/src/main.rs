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
//! * never a secondary GPT that fails structural or layout sanity checks
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
mod espscan;
mod fwcrc;
mod gfx;
mod nvram;
mod selfdev;
mod store;
mod ui;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use blockdev::UefiDisk;
use fwcrc::FirmwareCrc32;
use gptcore::backup::{self, Timestamp};
use gptcore::repair::{analyze, apply, plan, Analysis, RepairPlan};
use gptcore::style::{bad, dim, good, key, line, title, warn, Line, Style};
use gptcore::{bootcfg, bootopt, layout, prevent, report};
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
/// rewrite: `EFI Boot Fixer` stands on its own. The binary is named from
/// the part that survives that deletion, so it never has to be renamed.
const APP_NAME: &str = "EFI Boot Fixer for Steam Deck - github.com/speed47/efi-boot-fixer";

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
///
/// A single disk is taken without asking. A menu of one is not a choice,
/// and on the hardware this was written for it is the normal case: every
/// operation opened with a press that could only mean "yes, that one".
/// What made it safe to drop is that the disk is now named in the header of
/// every screen the operation goes on to draw — see [`ui::working_on`] — so
/// nothing is authorised without the target in front of the operator.
fn pick_from(title: &str, disks: Vec<Disk>) -> Option<Disk> {
    if disks.len() == 1 {
        return disks.into_iter().next();
    }
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
    let choice = ui::menu(title, &intro, &items, "back")?;
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

/// "1 entry" or "3 entries": the overview counts a great many things, and
/// a screen someone reads while their machine is broken should not also be
/// telling them it found 1 entries.
fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Everything the tool can find out without writing, on one page.
///
/// The question people actually arrive with is not "is my GPT valid" but
/// "why will this thing not boot", and answering it used to mean knowing in
/// advance which of the two halves of the tool to look in. Both halves had
/// a diagnostic already; what was missing was one screen that runs both and
/// then names the menu to go to.
fn run_overview(boot_device: &BootDevice) {
    let mut lines = Vec::new();
    // Built alongside the findings, so the advice at the bottom can only
    // say things this run actually observed.
    let mut next: Vec<Line> = Vec::new();

    let disks = scan(boot_device);
    lines.push(title("  Disks"));
    if disks.is_empty() {
        lines.push(bad("    No fixed, writable, whole disk was found."));
    }
    for disk in &disks {
        lines.push(key(format!("    {}", disk.label())));
        match &disk.analysis {
            Ok(analysis) => {
                lines.push(Line::new(
                    format!("      GPT: {}", report::verdict_line(analysis.verdict)),
                    report::verdict_style(analysis.verdict),
                ));
                if plan(analysis, &CRC).is_some() {
                    next.push(warn(format!(
                        "  Disk {} has a defect this tool can fix.",
                        disk.number
                    )));
                    next.push(dim("    Partition tables (GPT) -> Repair main GPT"));
                }
            }
            Err(e) => lines.push(bad(format!("      GPT: could not be read - {e}"))),
        }
    }

    let state = nvram::read();
    lines.push(Line::blank());
    lines.push(title("  Boot entries in NVRAM"));
    if let Some(e) = &state.truncated {
        lines.push(bad(format!("    {e}")));
    }
    if state.entries.is_empty() {
        lines.push(bad("    There are no boot entries at all."));
    } else {
        lines.push(line(format!(
            "    {} in the store",
            count(state.entries.len(), "entry", "entries")
        )));
    }
    match &state.order {
        Err(e) => lines.push(bad(format!("    BootOrder cannot be read - {e}"))),
        Ok(order) if order.is_empty() => {
            lines.push(bad("    The boot order is empty, so the firmware has no"));
            lines.push(bad("    list to try."));
            next.push(warn("  Nothing is in the boot order."));
            next.push(dim("    Boot entries (NVRAM) -> Set the default boot entry"));
        }
        Ok(order) => {
            lines.push(line(format!(
                "    {} in the boot order",
                count(order.len(), "entry", "entries")
            )));
            lines.push(dim(format!("      first: {}", slot_with_name(&state, order[0]))));
        }
    }
    if let Some(slot) = state.current {
        lines.push(dim(format!("      this boot used {}", slot_with_name(&state, slot))));
    }
    let unreadable = state.entries.iter().filter(|(_, e)| e.is_err()).count();
    if unreadable > 0 {
        lines.push(bad(format!("    {} will not decode", count(unreadable, "entry", "entries"))));
    }
    let orphans = state.orphans();
    if !orphans.is_empty() {
        lines.push(warn(format!(
            "    {} not in the boot order, and will not be offered",
            count(orphans.len(), "entry is", "entries are")
        )));
        next.push(warn("  An installed entry has fallen out of the boot order."));
        next.push(dim("    Boot entries (NVRAM) -> Set the default boot entry"));
    }

    let esps = espscan::scan(boot_device, &known_paths(&state));
    lines.push(Line::blank());
    lines.push(title("  Bootloaders on the ESPs"));
    if esps.volumes.is_empty() {
        lines.push(bad("    No EFI System Partition was found on a fixed disk."));
    } else {
        lines.push(line(format!(
            "    {} holding {}",
            count(esps.volumes.len(), "ESP", "ESPs"),
            count(esps.candidates.len(), "bootloader", "bootloaders")
        )));
        if !esps.unreadable.is_empty() {
            lines.push(bad(format!(
                "    {} could not be opened",
                count(esps.unreadable.len(), "ESP", "ESPs")
            )));
        }
        let unregistered = esps.candidates.iter().filter(|c| c.registered.is_none()).count();
        if unregistered > 0 {
            lines.push(warn(format!(
                "    {} nothing in NVRAM points at",
                count(unregistered, "loader", "loaders")
            )));
            next.push(warn("  A loader on an ESP has no boot entry pointing at it."));
            next.push(dim("    Boot entries (NVRAM) -> Register a bootloader"));
        }
    }

    lines.push(Line::blank());
    if next.is_empty() {
        lines.push(good("  Nothing here looks broken."));
    } else {
        lines.push(title("  What to do next"));
        lines.extend(next);
    }
    lines.push(Line::blank());
    lines.push(good("  Nothing was written. This screen never modifies anything."));
    ui::page("Check this machine (read only)", &lines);
}

/// Read-only: say what is wrong, write nothing, offer nothing.
fn run_check(boot_device: &BootDevice) {
    let Some(disk) = pick_disk("Check GPT", boot_device) else {
        return;
    };
    let _on = ui::working_on(disk.label());
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Check GPT", e.clone()),
    };

    let mut lines = ui::wrapped(&format!("  {}", disk.path), Style::Dim, "    ");
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

/// Rebuild a corrupt main GPT from the secondary GPT.
fn run_repair(boot_device: &BootDevice, esp_lost: &mut bool) {
    let Some(disk) = pick_disk("Repair main GPT", boot_device) else {
        return;
    };
    let _on = ui::working_on(disk.label());
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Repair", e.clone()),
    };

    let repair = plan(analysis, &CRC);
    if !ui::page("Repair main GPT", &report::render(analysis, repair.as_ref())) {
        return;
    }
    let Some(repair) = repair else {
        return;
    };

    // Say what is actually about to happen. "Rewrites the partition table"
    // is alarming and, when only the protective MBR is wrong, untrue.
    let warning = if analysis.verdict == gptcore::Verdict::MbrOnly {
        alloc::vec![
            warn("  This rewrites the protective MBR on this disk."),
            line("  Both GPTs are intact and are left alone."),
        ]
    } else {
        alloc::vec![
            bad("  This REWRITES the partition table on this disk."),
            line("  The proposed table came from the secondary GPT and was"),
            line("  shown on the previous screen."),
        ]
    };
    if !ui::confirm_sequence("Authorise write", &warning) {
        return show_note("Repair", "Cancelled. Nothing was written.".to_string());
    }
    let result = execute(&disk, &repair);
    report_write("Repair", &disk, result, esp_lost);
}

// ------------------------------------------------- where the files go

/// One volume, for a list the operator reads: what it is called, how much
/// room is left on it, and where it is attached.
fn volume_lines(volume: &store::Volume) -> Vec<Line> {
    let mut out = alloc::vec![key(format!("    {}", volume.name))];
    if let Some(free) = volume.free {
        out.push(dim(format!("      {} free", report::human_size(free))));
    }
    out.extend(ui::wrapped(&format!("      {}", volume.path), Style::Dim, "        "));
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Both,
    Esp,
    Removable,
}

/// Ask where a snapshot should be written.
///
/// With nothing removable attached there is no question worth asking: the
/// ESP is the only answer, and a menu whose every row does the same thing
/// is a press the operator has to make for nothing. They are told instead,
/// by [`attach_hint`], on the screen that reports what was written — where
/// it is advice for next time rather than an obstacle now.
///
/// `None` means the operator backed out, and nothing should be written.
fn choose_destinations(title: &str) -> Option<Vec<store::Volume>> {
    let mut removable = store::removable();
    if removable.is_empty() {
        return Some(alloc::vec![store::Volume::boot()]);
    }

    // Two sticks is a rare case, and folding it into the destination menu
    // would make every row's meaning depend on which of them is meant. It
    // is a separate question, asked first, and only when it exists.
    let stick = if removable.len() == 1 {
        removable.remove(0)
    } else {
        let items: Vec<ui::Item> = removable
            .iter()
            .map(|v| ui::Item::with_detail(v.name.clone(), volume_lines(v)))
            .collect();
        let intro = alloc::vec![
            dim("  More than one removable volume is attached."),
            dim("  A snapshot can go to one of them, not to all."),
        ];
        let chosen = ui::menu("Which removable volume?", &intro, &items, "back")?;
        removable.remove(chosen)
    };

    // Built here rather than as literals because the rows name the volume:
    // "Save to SD_CARD only" is a choice, "Save to the removable volume
    // only" is a thing to work out.
    let removable_only = format!("Save to {} only", stick.name);
    let on_stick = format!("Write it to {} and nowhere else.", stick.name);
    let and_stick = format!("One copy on the ESP, one on {}.", stick.name);
    let menu = Menu::new(alloc::vec![
        row(
            Dest::Both,
            "Save to both",
            &[
                &and_stick,
                "The ESP copy is the one this program can always find",
                "again; the other one survives the disk.",
            ]
        ),
        row(
            Dest::Esp,
            "Save to the ESP only",
            &["Write it next to this program, as earlier versions", "always did."]
        ),
        row(Dest::Removable, &removable_only, &[&on_stick, "Nothing is written to the ESP."]),
    ]);

    let intro = alloc::vec![
        dim("  Removable media is attached, so a copy can be kept"),
        dim("  somewhere other than this machine's own disk."),
    ];
    match menu.show(title, &intro, "back")? {
        Dest::Both => Some(alloc::vec![store::Volume::boot(), stick]),
        Dest::Esp => Some(alloc::vec![store::Volume::boot()]),
        Dest::Removable => Some(alloc::vec![stick]),
    }
}

/// True if no destination is removable — every copy stays in the machine.
///
/// Not the same as "the ESP only": an image launched from a rescue stick
/// has a removable launch volume, and a copy written there is already an
/// off-device one. That is why the question is asked of the volume rather
/// than of which menu row was chosen.
fn on_disk_only(dests: &[store::Volume]) -> bool {
    !dests.iter().any(|v| v.removable)
}

/// What to say to someone who has just saved a snapshot to the ESP alone.
///
/// Empty unless there was nothing else on offer. Telling an operator who
/// deliberately chose the ESP that they should have chosen otherwise is
/// nagging; telling one who was never asked is the missing half of the
/// question, and it is the only place the question can be asked — there is
/// nothing to attach media *to* on a screen that has already been drawn.
fn attach_hint(dests: &[store::Volume]) -> Vec<Line> {
    if !on_disk_only(dests) || !store::removable().is_empty() {
        return Vec::new();
    }
    alloc::vec![
        Line::blank(),
        dim("  No removable media is attached. Plug in a USB stick or"),
        dim("  an SD card before coming here, and a copy can be written"),
        dim("  to that as well."),
    ]
}

/// What came of writing one snapshot to every chosen destination.
struct Written {
    ok: Vec<Line>,
    failed: Vec<Line>,
}

impl Written {
    fn any(&self) -> bool {
        !self.ok.is_empty()
    }

    /// The paths written, then the destinations that refused, in one block.
    fn lines(&self) -> Vec<Line> {
        let mut out = self.ok.clone();
        if !self.failed.is_empty() {
            if !out.is_empty() {
                out.push(Line::blank());
                out.push(warn("  But not everywhere:"));
            }
            out.extend(self.failed.iter().cloned());
        }
        out
    }
}

/// Choose a free name and write `bytes` under it on every destination.
///
/// `pick` is the naming scheme — `gpt.NNN` or `boot.NNN` — and is handed
/// every filename in use on **every volume the tool can see**, not merely
/// on the ones being written to. Numbering from the wider view is what
/// stops one name meaning two things: an operator who saves to the stick
/// now and to the ESP later would otherwise get a `gpt.001` in each place
/// holding a different table, and the restore screen would offer two rows
/// that are impossible to tell apart by name.
///
/// A destination whose directory cannot be listed is **dropped, not fatal**.
/// Numbering from a listing that failed could collide with a snapshot nobody
/// can see, so that volume must not be written to — but the others still
/// can, and the alternative is worse than it sounds: on the NVRAM path a
/// misbehaving stick would otherwise talk the operator into changing boot
/// variables with no saved copy at all, on a machine whose ESP was writable
/// the whole time.
///
/// Every remaining destination is attempted even after one has failed, and
/// both outcomes are carried back. A stick that turned out to be full must
/// not hide the copy that did land on the ESP, and a failure on the ESP must
/// not throw away the knowledge that the off-device copy exists.
fn write_snapshot(
    dests: &[store::Volume],
    pick: impl Fn(&[String]) -> Result<String, String>,
    bytes: &[u8],
) -> Result<Written, String> {
    let mut usable = Vec::new();
    let mut taken = Vec::new();
    let mut failed = Vec::new();
    for volume in dests {
        match volume.names() {
            Ok(names) => {
                taken.extend(names);
                usable.push(volume);
            }
            Err(e) => failed.push(bad(format!("    {} - {e}", volume.name))),
        }
    }
    if usable.is_empty() {
        return Ok(Written { ok: Vec::new(), failed });
    }

    // Everything attached, for numbering only, and read separately from the
    // destinations above: a volume nothing is being written to has a say in
    // what the file is called and none at all in whether it gets written, so
    // a listing that fails here is simply ignored.
    for volume in source_volumes() {
        taken.extend(volume.names().unwrap_or_default());
    }

    let name = pick(&taken)?;
    let mut written = Written { ok: Vec::new(), failed };
    for volume in usable {
        match volume.save(&name, bytes) {
            Ok(path) => {
                written.ok.push(key(format!("    {path}")));
                written.ok.push(dim(format!("      on {}", volume.name)));
            }
            Err(e) => written.failed.push(bad(format!("    {} - {e}", volume.name))),
        }
    }
    Ok(written)
}

/// The next free `gpt.NNN`, given every name already in use.
fn gpt_name(taken: &[String]) -> Result<String, String> {
    backup::next_name(taken).ok_or_else(|| {
        format!(
            "\\{}\\ already holds gpt.{}; delete some snapshots first",
            store::DIR,
            backup::MAX_SEQUENCE
        )
    })
}

/// The next free `boot.NNN`, given every name already in use.
fn boot_name(taken: &[String]) -> Result<String, String> {
    bootcfg::next_name(taken).ok_or_else(|| {
        format!(
            "\\{}\\ already holds boot.{}; delete some first",
            store::DIR,
            bootcfg::MAX_SEQUENCE
        )
    })
}

/// Every volume worth looking for saved files on.
///
/// Reading asks no question: a snapshot the operator saved to a stick is
/// one they expect to be offered, and a stick that is not attached simply
/// contributes nothing.
fn source_volumes() -> Vec<store::Volume> {
    let mut all = alloc::vec![store::Volume::boot()];
    all.extend(store::removable());
    all
}

/// Save both GPTs to the ESP, to removable media, or to both.
fn run_backup(boot_device: &BootDevice, esp_lost: bool) {
    if esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    let Some(disk) = pick_disk("Back up GPT", boot_device) else {
        return;
    };
    let _on = ui::working_on(disk.label());
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

    let Some(dests) = choose_destinations("Where should the snapshot go?") else {
        return;
    };

    let mut lines = backup::describe(&archive);
    lines.push(Line::blank());
    lines.push(line(format!("  {} bytes will be written to \\{}\\ on:", bytes.len(), store::DIR)));
    for volume in &dests {
        lines.extend(volume_lines(volume));
    }
    // The caveat only applies to a copy on the ESP, and only when the ESP
    // is on the disk being backed up. Once a removable copy is being made
    // too, the same fact stops being a warning and becomes the reason the
    // other copy is worth having.
    if disk.boot && dests.iter().any(|v| !v.removable) {
        lines.push(Line::blank());
        if on_disk_only(&dests) {
            lines.push(warn("  NOTE: the ESP is on this same disk, so this is a"));
            lines.push(warn("  convenience copy, not an off-device backup. Copy the"));
            lines.push(warn("  file elsewhere once you can boot again."));
        } else {
            lines.push(line("  The ESP is on the disk being backed up, so it is the"));
            lines.push(line("  removable copy that survives losing this disk."));
        }
    }
    if !ui::page("Back up GPT", &lines) {
        return;
    }

    // Writing a file needs no confirmation sequence: it creates data, it
    // does not overwrite a partition table, and it never replaces an
    // existing snapshot.
    let written = match write_snapshot(&dests, gpt_name, &bytes) {
        Ok(written) => written,
        Err(e) => return show_error("Back up GPT FAILED", e),
    };
    if !written.any() {
        let mut lines = alloc::vec![bad("  Nothing was written:")];
        lines.extend(written.lines());
        return ui::message("Back up GPT FAILED", &lines);
    }

    let mut lines = alloc::vec![good("  Saved as:")];
    lines.extend(written.lines());
    lines.push(Line::blank());
    lines.push(dim("  Restore offers whatever it finds, on the ESP and on"));
    lines.push(dim("  any removable media attached at the time."));
    lines.extend(attach_hint(&dests));
    ui::message("Back up GPT", &lines);
}

/// What was true when a snapshot was taken, for whoever reads it later.
///
/// None of this is needed to restore the file. It is here for the question
/// asked years afterwards: is this from this machine, and what was it?
fn provenance(disk: &Disk, boot_device: &BootDevice) -> Vec<(String, String)> {
    let mut meta = alloc::vec![(
        String::from("tool"),
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("BOOTFIXR_VERSION"))
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

/// A snapshot found on one of the volumes, with whatever we can work out
/// about where it came from.
struct Saved {
    name: String,
    /// The volume it was found on. Carried because two volumes can each
    /// hold a `gpt.001`, and then the name alone names two files.
    source: String,
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
fn attribute(saved: &mut [Saved], disks: &[Disk]) {
    for entry in saved.iter_mut() {
        let mut best: Option<(usize, backup::Comparison)> = None;
        for (i, disk) in disks.iter().enumerate() {
            let Ok(analysis) = &disk.analysis else { continue };
            let c = backup::compare(&entry.archive, analysis);
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
        entry.best = best;
    }
}

/// What was found in `\BOOTFIXR` on every volume: the snapshots that
/// decoded, and a readable complaint about each one that did not.
struct Found {
    usable: Vec<Saved>,
    rejected: Vec<Line>,
}

/// Read and decode every snapshot on every volume, reporting the ones that
/// cannot be used rather than quietly dropping them.
///
/// A volume that cannot be listed is a rejection like any other and not a
/// failure of the whole screen: an unreadable stick must not stand between
/// the operator and the snapshots sitting on the ESP. It is still said out
/// loud, because "there are no snapshots" and "I could not look" are
/// different answers.
fn load_saved() -> Found {
    let mut found = Found { usable: Vec::new(), rejected: Vec::new() };
    for volume in source_volumes() {
        let files = match volume.list() {
            Ok(files) => files,
            Err(e) => {
                found.rejected.push(bad(format!("  {} - {e}", volume.name)));
                continue;
            }
        };
        for file in files {
            let data = match file.data {
                Ok(data) => data,
                Err(e) => {
                    found.rejected.push(bad(format!("  {} on {} - {e}", file.name, volume.name)));
                    continue;
                }
            };
            match backup::decode(&data, &CRC) {
                Ok(archive) => found.usable.push(Saved {
                    name: file.name,
                    source: volume.name.clone(),
                    archive,
                    best: None,
                }),
                Err(e) => {
                    found.rejected.push(bad(format!("  {} on {} - {e}", file.name, volume.name)))
                }
            }
        }
    }
    found
}

/// Put a saved snapshot back.
fn run_restore(boot_device: &BootDevice, esp_lost: &mut bool) {
    if *esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    let Found { mut usable, rejected } = load_saved();

    if usable.is_empty() {
        let mut lines = alloc::vec![
            warn(format!("  No usable snapshots in \\{}\\, on the ESP or on", store::DIR)),
            warn("  any removable media attached now."),
        ];
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
    attribute(&mut usable, &disks);
    let saved = usable;

    let intro = alloc::vec![
        dim(format!("  {} snapshot(s) in \\{}\\ on the volumes here.", saved.len(), store::DIR)),
        dim("  A snapshot only fits the disk it was taken from."),
    ];
    let items: Vec<ui::Item> = saved
        .iter()
        .map(|sv| {
            ui::Item::with_detail(
                sv.label(),
                alloc::vec![
                    sv.belongs_to(&disks),
                    // Before the disk GUID, because with the same name on
                    // two volumes this is the line that says which row is
                    // which.
                    dim(format!("  found on {}", sv.source)),
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
        match ui::menu_inspectable("Restore GPT", &intro, &items, "back", "details", selected) {
            ui::Choice::Cancelled => return,
            ui::Choice::Item(i) => break &saved[i].archive,
            ui::Choice::Inspect(i) => {
                selected = i;
                let sv = &saved[i];
                let mut lines = alloc::vec![
                    key(format!("  {}", sv.name)),
                    dim(format!("  found on {}", sv.source)),
                    Line::blank(),
                ];
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
    let _on = ui::working_on(disk.label());
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
    let disk_name = format!("Disk {}", disk.number);
    let mut lines = backup::inspect(archive, Some((disk_name.as_str(), &comparison)));
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

    let mut warning =
        alloc::vec![bad("  This REPLACES both partition tables with the saved copy.")];
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
    let _on = ui::working_on(disk.label());
    let analysis = match &disk.analysis {
        Ok(a) => a,
        Err(e) => return show_error("Prevent", e.clone()),
    };

    let verdict = prevent::assess(analysis);
    if !ui::page("Prevent recurrence", &prevent::describe(verdict)) {
        return;
    }
    if !verdict.will_write() {
        return;
    }
    let Some(gap_plan) = prevent::plan(analysis, &CRC) else {
        return;
    };

    if !ui::page("Prevent recurrence: what will be written", &report::render_plan(&gap_plan)) {
        return;
    }

    let warning = alloc::vec![
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

/// The device path of every entry that decoded, for `espscan` to match a
/// file on an ESP against.
fn known_paths(state: &nvram::BootState) -> Vec<(u16, Vec<u8>)> {
    state
        .entries
        .iter()
        .filter_map(|(slot, e)| e.as_ref().ok().map(|o| (*slot, o.device_path.clone())))
        .collect()
}

/// What is actually installed on the ESPs, and whether NVRAM knows about it.
fn run_boot_scan(boot_device: &BootDevice) {
    let state = nvram::read();
    let scan = espscan::scan(boot_device, &known_paths(&state));

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

/// Save the whole boot configuration to the chosen volumes.
fn take_boot_snapshot(dests: &[store::Volume]) -> Result<Written, String> {
    let (vars, missed) = nvram::capture();
    if vars.is_empty() {
        return Err(String::from("there is nothing in NVRAM to save"));
    }
    let mut meta = alloc::vec![(
        String::from("tool"),
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("BOOTFIXR_VERSION"))
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

    let snap = bootcfg::Snapshot { time: now(), vars, meta };
    let bytes = bootcfg::encode(&snap, &CRC);
    write_snapshot(dests, boot_name, &bytes)
}

/// Save the boot configuration before this session's first NVRAM write.
///
/// Mandatory, but not a hard refusal when it fails. On a machine whose ESP
/// has gone unreachable, changing a boot entry may be the only remedy
/// left, and refusing outright would make the tool useless exactly when it
/// is needed. So a failure becomes a question with the consequence spelled
/// out, and `taken` stays false so the next write tries again.
///
/// Where it goes is asked here rather than assumed, exactly as it is for a
/// GPT snapshot: this copy is the only record of what NVRAM looked like
/// before the session touched it, and a stick is where it is least likely
/// to be lost. Asked once per session, because that is how often the
/// snapshot is taken.
fn ensure_boot_snapshot(taken: &mut bool, esp_lost: bool) -> bool {
    if *taken {
        return true;
    }
    if esp_lost && !warn_esp_may_be_gone() {
        return false;
    }
    let Some(dests) = choose_destinations("Where should the boot configuration go?") else {
        return false;
    };
    // Nowhere would take it is the same situation as not having been able
    // to build it at all, and gets the same question.
    let written = match take_boot_snapshot(&dests) {
        Ok(written) if written.any() => Ok(written),
        Ok(refused) => Err(refused.lines()),
        Err(e) => Err(alloc::vec![bad(format!("  {e}"))]),
    };

    match written {
        Ok(written) => {
            *taken = true;
            let mut lines =
                alloc::vec![good("  The boot configuration as it is now was saved to:")];
            lines.extend(written.lines());
            lines.push(Line::blank());
            lines.push(dim("  Taken automatically before the first change of"));
            lines.push(dim("  this session, so there is a record to go back to."));
            lines.extend(attach_hint(&dests));
            ui::message("Boot configuration saved", &lines);
            true
        }
        Err(why) => {
            let mut lines = why;
            lines.push(Line::blank());
            lines.push(warn("  Nothing has been changed yet. Continuing means"));
            lines.push(warn("  making the change with no saved copy of what the"));
            lines.push(warn("  boot configuration looks like right now."));
            lines.push(Line::blank());
            lines.push(key("  Continue without a saved copy?"));
            ui::page("Could not save the boot configuration", &lines)
        }
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
    ui::menu(title, intro, &items, "back").map(|i| slots[i])
}

/// Put a loader found on an ESP into NVRAM.
fn run_boot_register(boot_device: &BootDevice, snapshot: &mut bool, esp_lost: bool) {
    let state = nvram::read();
    let scan = espscan::scan(boot_device, &known_paths(&state));

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
    let Some(chosen) = ui::menu("Register a bootloader", &intro, &items, "back") else {
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
    let Some(placement) = ui::menu("Where in the boot order?", &[], &where_items, "back") else {
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

/// Put a saved boot configuration back into NVRAM.
///
/// The counterpart to the snapshot [`ensure_boot_snapshot`] takes before the
/// session's first change. Until this existed the tool wrote those files and
/// offered no way to use one, which made the promise they represent — that
/// an NVRAM edit made from a screen with no keyboard has something behind
/// it — true only for someone with a second machine and a hex editor.
fn run_boot_restore(snapshot: &mut bool, esp_lost: bool) {
    const TITLE: &str = "Restore the boot configuration";
    if esp_lost && !warn_esp_may_be_gone() {
        return;
    }
    // Same rule as the GPT snapshots: every volume is looked at, and a
    // volume that will not open is a rejection rather than the end of the
    // screen.
    let mut usable: Vec<(String, String, bootcfg::Snapshot)> = Vec::new();
    let mut rejected: Vec<Line> = Vec::new();
    for volume in source_volumes() {
        let files = match volume.list_boot() {
            Ok(files) => files,
            Err(e) => {
                rejected.push(bad(format!("  {} - {e}", volume.name)));
                continue;
            }
        };
        for file in files {
            match file.data.and_then(|d| bootcfg::decode(&d, &CRC).map_err(|e| e.to_string())) {
                Ok(snap) => usable.push((file.name, volume.name.clone(), snap)),
                Err(e) => rejected.push(bad(format!("  {} on {} - {e}", file.name, volume.name))),
            }
        }
    }

    if usable.is_empty() {
        let mut lines = alloc::vec![
            warn(format!("  No usable boot snapshots in \\{}\\, on the ESP or", store::DIR)),
            warn("  on any removable media attached now."),
            Line::blank(),
            dim("  One is taken automatically before the first change"),
            dim("  of a session, so there is nothing here until then."),
        ];
        if !rejected.is_empty() {
            lines.push(Line::blank());
            lines.push(warn("  Rejected:"));
            lines.extend(rejected);
        }
        return ui::message(TITLE, &lines);
    }
    if !rejected.is_empty() {
        let mut lines = alloc::vec![
            warn("  Some files could not be read and are not offered"),
            warn("  below:"),
            Line::blank(),
        ];
        lines.extend(rejected);
        ui::message(TITLE, &lines);
    }

    let items: Vec<ui::Item> = usable
        .iter()
        .map(|(name, source, snap)| {
            ui::Item::with_detail(
                format!("{:<9} {}", name, bootcfg::summary(snap)),
                alloc::vec![
                    dim(format!("  found on {source}")),
                    dim(match snap.meta_get("tool") {
                        Some(t) => format!("  written by {t}"),
                        None => String::from("  written by an older build"),
                    }),
                    // The one thing that makes a snapshot the wrong one to
                    // put back: it was taken on a different machine.
                    dim(match snap.meta_get("firmware") {
                        Some(f) => format!("  on {f}"),
                        None => String::from("  firmware not recorded"),
                    }),
                ],
            )
        })
        .collect();
    let intro = alloc::vec![
        dim("  Saved copies of the boot entries and the boot order."),
        dim("  Restoring one writes those variables back as they were."),
    ];
    let Some(chosen) = ui::menu(TITLE, &intro, &items, "back") else {
        return;
    };
    let (name, source, snap) = &usable[chosen];

    let writes = bootcfg::plan_restore(snap);
    if writes.is_empty() {
        return show_note(TITLE, format!("{name} holds no variables to write."));
    }

    let mut review =
        alloc::vec![key(format!("  {name}")), dim(format!("  found on {source}")), Line::blank()];
    review.extend(bootcfg::describe(snap));
    review.push(Line::blank());
    review.push(warn("  Only the variables listed above are written. An entry"));
    review.push(warn("  added since this was taken is left alone, not removed:"));
    review.push(warn("  this puts back what was saved, it does not put NVRAM"));
    review.push(warn("  back exactly as it was."));

    let warning = alloc::vec![
        key(format!("  {name}   taken {}", snap.time)),
        Line::blank(),
        bad("  This OVERWRITES the boot entries and the boot order"),
        bad("  named on the previous screen."),
        line("  No disk is written to."),
    ];
    authorise_and_write(TITLE, review, warning, &writes, snapshot, esp_lost);
}

// ---------------------------------------------------------- the menus

/// A menu's rows, each carrying what choosing it does.
///
/// Every menu here used to be a list of labels and a `match` on the index it
/// returned, with a comment explaining which number was Exit. That is two
/// lists nothing checks against each other, and inserting a row silently
/// wires it to its neighbour's action. An action carried on the row cannot
/// drift from it, which is what makes reordering a menu a safe edit.
struct Menu<A> {
    actions: Vec<A>,
    items: Vec<ui::Item>,
}

impl<A: Copy> Menu<A> {
    fn new(rows: Vec<(A, ui::Item)>) -> Self {
        let (actions, items) = rows.into_iter().unzip();
        Menu { actions, items }
    }

    /// Show it, and say what was chosen. `None` if the operator backed out.
    fn show(&self, title: &str, intro: &[Line], back: &str) -> Option<A> {
        ui::menu(title, intro, &self.items, back).map(|i| self.actions[i])
    }
}

/// One row: what it does, what it is called, and the help shown under the
/// list while it is selected.
fn row<A>(action: A, label: &str, detail: &[&str]) -> (A, ui::Item) {
    (action, ui::Item::with_detail(label, detail.iter().map(|t| dim(format!("  {t}"))).collect()))
}

/// Marks a row that opens another menu rather than doing something.
///
/// Cheap, and it is the whole difference between a list of five operations
/// and a list of three operations and two doors.
const SUBMENU: &str = "  >";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gpt {
    Check,
    Backup,
    Restore,
    Repair,
    Prevent,
}

/// Everything that reads or writes a partition table.
///
/// Ordered by what it costs to be wrong: the check writes nothing, the
/// backup writes a file, and the last three rewrite a table. Backing up
/// sits above the three that overwrite because that is the order the
/// operations should be done in, and a menu is the cheapest place to say so.
fn run_gpt_menu(boot_device: &BootDevice, esp_lost: &mut bool) {
    let menu = Menu::new(alloc::vec![
        row(
            Gpt::Check,
            "Check a disk's GPT",
            &["Read both tables and report what is wrong.", "Writes nothing."]
        ),
        row(
            Gpt::Backup,
            "Back up both GPTs to a file",
            &[
                "Save the tables to the ESP, to removable media, or",
                "to both, so they can be put back as they are now."
            ]
        ),
        row(
            Gpt::Restore,
            "Restore GPTs from a saved backup",
            &["Write a previously saved snapshot back onto the", "disk it was taken from."]
        ),
        row(
            Gpt::Repair,
            "Repair main GPT from the secondary",
            &["Rebuild a corrupt main table from the secondary GPT", "at the end of the disk."]
        ),
        row(
            Gpt::Prevent,
            "Prevent recurrence (experimental)",
            &[
                "Lower FirstUsableLBA so the Windows arithmetic that caused",
                "the corruption produces the right answer for this GPT.",
                "This is unproven. Read docs/corruption.md first."
            ]
        ),
    ]);

    let intro = alloc::vec![
        dim("  Checking reads only; backing up writes one file."),
        dim("  The rest rewrite a table, and show what first."),
    ];
    loop {
        match menu.show("Partition tables (GPT)", &intro, "back") {
            Some(Gpt::Check) => run_check(boot_device),
            Some(Gpt::Backup) => run_backup(boot_device, *esp_lost),
            Some(Gpt::Restore) => run_restore(boot_device, esp_lost),
            Some(Gpt::Repair) => run_repair(boot_device, esp_lost),
            Some(Gpt::Prevent) => run_prevent(boot_device, esp_lost),
            None => return,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Nvram {
    View,
    Scan,
    Register,
    Default,
    Once,
    Restore,
}

/// Everything that reads or writes the firmware's boot configuration.
///
/// `snapshot` is the session's "a copy of NVRAM has been saved" flag, and it
/// belongs to `main` rather than to this function: it used to be declared
/// here, which meant leaving this menu and coming back took a second
/// snapshot of a configuration this session had already changed.
fn run_nvram_menu(boot_device: &BootDevice, snapshot: &mut bool, esp_lost: bool) {
    let menu = Menu::new(alloc::vec![
        row(
            Nvram::View,
            "View the boot entries",
            &["What the firmware will try, in order, plus any", "entry that has fallen out of it."]
        ),
        row(
            Nvram::Scan,
            "Scan the ESPs for bootloaders",
            &[
                "Which loaders are installed on disk, and whether",
                "NVRAM has an entry pointing at each one."
            ]
        ),
        row(
            Nvram::Register,
            "Register a bootloader",
            &[
                "Add a boot entry for a loader that is on an ESP",
                "but that nothing in NVRAM points at."
            ]
        ),
        row(
            Nvram::Default,
            "Set the default boot entry",
            &[
                "Move an entry to the front of the boot order.",
                "Changes the order only; no entry is rewritten."
            ]
        ),
        row(
            Nvram::Once,
            "Boot something once (next boot only)",
            &["Try an entry without committing to it. The", "firmware clears this as it uses it."]
        ),
        row(
            Nvram::Restore,
            "Restore the boot configuration",
            &[
                "Write back a saved copy of the entries and the",
                "boot order, from a snapshot on any volume here."
            ]
        ),
    ]);

    let intro = alloc::vec![
        dim("  The first two screens read only."),
        dim("  The rest change NVRAM, and say so before they do."),
    ];
    loop {
        match menu.show("Boot entries (NVRAM)", &intro, "back") {
            Some(Nvram::View) => run_boot_view(),
            Some(Nvram::Scan) => run_boot_scan(boot_device),
            Some(Nvram::Register) => run_boot_register(boot_device, snapshot, esp_lost),
            Some(Nvram::Default) => run_boot_default(snapshot, esp_lost),
            Some(Nvram::Once) => run_boot_once(snapshot, esp_lost),
            Some(Nvram::Restore) => run_boot_restore(snapshot, esp_lost),
            None => return,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Main {
    Overview,
    Gpt,
    Nvram,
    Reboot,
    Exit,
}

/// The top level: one diagnostic, two doors, and the way out.
///
/// Grouped by what an operation acts on rather than by the order the
/// features were built in. The version before this had five GPT operations
/// flat and five NVRAM operations behind a submenu, so two halves of the
/// same tool sat at different depths for no reason anyone reading the menu
/// could see. The overview is first because it is what answers the question
/// someone arrives with, and it ends by naming the door to go through.
fn main_menu() -> Menu<Main> {
    Menu::new(alloc::vec![
        row(
            Main::Overview,
            "Check this machine (read only)",
            &[
                "Every disk's partition table, the firmware's boot",
                "list, and what is on the ESPs."
            ]
        ),
        row(
            Main::Gpt,
            &format!("Partition tables (GPT){SUBMENU}"),
            &["Check, back up, restore or repair the partition", "tables on a disk."]
        ),
        row(
            Main::Nvram,
            &format!("Boot entries (NVRAM){SUBMENU}"),
            &[
                "What the firmware will try to boot, and the",
                "loaders on the ESPs it could point at."
            ]
        ),
        row(
            Main::Reboot,
            "Reboot",
            &["Restart the machine, so the firmware reads the", "disks and the boot list again."]
        ),
        row(Main::Exit, "Exit", &["Return to the firmware."]),
    ])
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
    let menu = main_menu();
    let mut esp_lost = false;
    // Saved once per session, before the first NVRAM change, by whichever
    // operation gets there first. Held here so that leaving the boot menu
    // and coming back does not take a second copy of a configuration this
    // session has already changed.
    let mut boot_snapshot = false;

    loop {
        // Rebuilt every time round rather than once: the device path is
        // wrapped to the screen width, and the display screen can change
        // that width from any submenu this returns from. Not from this
        // menu's own display screen, though: the menu loop handles View
        // itself and never hands control back here, so a width changed
        // there leaves these lines truncated until a submenu is entered
        // and left again.
        let mut intro = alloc::vec![dim(format!("  version {}", env!("BOOTFIXR_VERSION")))];
        if boot_device.is_known() {
            intro.extend(ui::wrapped(
                &format!("  launched from {}", path_text(boot_device.path())),
                Style::Dim,
                "    ",
            ));
        } else {
            intro.push(warn("  boot volume unknown - no disk will be marked [boot]"));
        }

        match menu.show(APP_NAME, &intro, "exit") {
            Some(Main::Overview) => run_overview(&boot_device),
            Some(Main::Gpt) => run_gpt_menu(&boot_device, &mut esp_lost),
            Some(Main::Nvram) => run_nvram_menu(&boot_device, &mut boot_snapshot, esp_lost),
            // Only comes back if the reboot was not confirmed.
            Some(Main::Reboot) => run_reboot(),
            Some(Main::Exit) | None => break,
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
