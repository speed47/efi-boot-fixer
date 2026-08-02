//! Whatever the firmware is willing to say about a drive's identity.
//!
//! There is no protocol that simply hands over "vendor and model".
//! `EFI_DISK_INFO_PROTOCOL` exposes the raw identification payload of
//! whichever transport the drive is on, and what that payload contains
//! differs per transport:
//!
//! * SCSI-like transports, which includes USB mass storage, answer
//!   `Inquiry()` with SCSI INQUIRY data — vendor at byte 8, product at 16.
//! * ATA and AHCI answer `Identify()` with ATA IDENTIFY DEVICE data, whose
//!   model string sits at byte 54 with every 16-bit word byte-swapped.
//! * NVMe answers `Identify()` with *namespace* data, which carries no
//!   model string at all, and refuses `Inquiry()`. So on the Deck's own
//!   NVMe drive there is nothing to show, and the list falls back to
//!   capacity and device path.
//!
//! Everything here is best-effort and returns `None` rather than guessing:
//! a wrong drive name in a list of disks you are about to overwrite is
//! worse than no name.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::c_void;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::unsafe_protocol;
use uefi::{guid, Guid, Handle, Status};

/// `EFI_DISK_INFO_PROTOCOL`, which the `uefi` crate does not wrap.
#[repr(C)]
pub struct DiskInfoProtocol {
    /// Identifies which transport, and therefore how to read the payload.
    pub interface: Guid,
    pub inquiry: unsafe extern "efiapi" fn(
        this: *const DiskInfoProtocol,
        buffer: *mut c_void,
        size: *mut u32,
    ) -> Status,
    pub identify: unsafe extern "efiapi" fn(
        this: *const DiskInfoProtocol,
        buffer: *mut c_void,
        size: *mut u32,
    ) -> Status,
    pub sense_data: unsafe extern "efiapi" fn(
        this: *const DiskInfoProtocol,
        buffer: *mut c_void,
        size: *mut u32,
        number: *mut u8,
    ) -> Status,
    pub which_ide: unsafe extern "efiapi" fn(
        this: *const DiskInfoProtocol,
        channel: *mut u32,
        device: *mut u32,
    ) -> Status,
}

#[repr(transparent)]
#[unsafe_protocol("d432a67f-14dc-484b-b3bb-3f0291849327")]
pub struct DiskInfo(DiskInfoProtocol);

const IDE_INTERFACE: Guid = guid!("5e948fe3-26d3-42b5-af17-610287188dec");
const AHCI_INTERFACE: Guid = guid!("9e498932-4abc-45af-a34d-0247787be7c6");

impl DiskInfo {
    fn call(
        &self,
        f: unsafe extern "efiapi" fn(*const DiskInfoProtocol, *mut c_void, *mut u32) -> Status,
        capacity: usize,
    ) -> Option<Vec<u8>> {
        let mut buf = alloc::vec![0u8; capacity];
        let mut size = capacity as u32;
        // SAFETY: the protocol is open for the duration of the call, and the
        // buffer is ours and at least `size` bytes long.
        let status = unsafe { f(&self.0, buf.as_mut_ptr().cast(), &mut size) };
        if status != Status::SUCCESS {
            return None;
        }
        buf.truncate((size as usize).min(capacity));
        Some(buf)
    }

    /// SCSI INQUIRY: vendor and product, space-padded ASCII.
    fn scsi_name(&self) -> Option<String> {
        let data = self.call(self.0.inquiry, 256)?;
        let vendor = ascii(data.get(8..16)?);
        let product = ascii(data.get(16..32)?);
        match (vendor.is_empty(), product.is_empty()) {
            (true, true) => None,
            (true, false) => Some(product),
            (false, true) => Some(vendor),
            (false, false) => Some(alloc::format!("{vendor} {product}")),
        }
    }

    /// ATA IDENTIFY DEVICE: model at word 27, i.e. bytes 54..94, with each
    /// word stored big-endian relative to the rest of the structure.
    fn ata_name(&self) -> Option<String> {
        let data = self.call(self.0.identify, 512)?;
        let raw = data.get(54..94)?;
        let mut swapped = Vec::with_capacity(raw.len());
        for pair in raw.chunks_exact(2) {
            swapped.push(pair[1]);
            swapped.push(pair[0]);
        }
        let name = ascii(&swapped);
        (!name.is_empty()).then_some(name)
    }
}

/// Printable ASCII only, trimmed. Anything else means we misread the
/// payload, and the caller should show nothing rather than mojibake.
fn ascii(bytes: &[u8]) -> String {
    let text: String = bytes
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '?' })
        .collect();
    if text.contains('?') {
        return String::new();
    }
    text.trim().to_string()
}

/// A human-readable drive name, if this transport offers one.
pub fn model(handle: Handle) -> Option<String> {
    // SAFETY: GetProtocol neither installs nor removes interfaces, and the
    // returned ScopedProtocol closes it again on drop.
    let info = unsafe {
        boot::open_protocol::<DiskInfo>(
            OpenProtocolParams { handle, agent: boot::image_handle(), controller: None },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()?;

    // Try the transport's own answer first, then the other one: firmware
    // implementations vary, and a wrong-looking payload is filtered by the
    // printability check rather than by trusting the interface GUID alone.
    let ata_first = info.0.interface == IDE_INTERFACE || info.0.interface == AHCI_INTERFACE;
    let name = if ata_first {
        info.ata_name().or_else(|| info.scsi_name())
    } else {
        info.scsi_name().or_else(|| info.ata_name())
    }?;

    // A name of one or two characters is noise, not an identification.
    (name.chars().count() >= 3).then_some(name)
}
