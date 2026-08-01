//! CRC-32 from the firmware.
//!
//! Using `gBS->CalculateCrc32` means the checksums we write are produced
//! by the same code that will validate them at the next boot. The `uefi`
//! crate does not wrap this service, so it is called through the raw boot
//! services table.
//!
//! If the call is unavailable or fails, we fall back to `gptcore`'s own
//! implementation rather than returning a wrong value, and say so.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use gptcore::crc::{Crc32, SoftCrc32};
use uefi::table::system_table_raw;
use uefi::Status;

static FELL_BACK: AtomicBool = AtomicBool::new(false);

/// True if any CRC was computed by the built-in implementation because the
/// firmware service could not be used.
pub fn used_fallback() -> bool {
    FELL_BACK.load(Ordering::Relaxed)
}

pub struct FirmwareCrc32;

impl Crc32 for FirmwareCrc32 {
    fn crc32(&self, data: &[u8]) -> u32 {
        // CalculateCrc32 rejects a zero length; the CRC of nothing is 0.
        if data.is_empty() {
            return 0;
        }
        match firmware_crc32(data) {
            Some(value) => value,
            None => {
                FELL_BACK.store(true, Ordering::Relaxed);
                SoftCrc32.crc32(data)
            }
        }
    }
}

fn firmware_crc32(data: &[u8]) -> Option<u32> {
    let system_table = system_table_raw()?;
    let mut out = 0u32;

    // SAFETY: `system_table_raw` returns the firmware's own system table.
    // Boot services are still live: this application never calls
    // ExitBootServices, so the table and the function pointer remain
    // valid. `data` is a live slice, and `out` is a local u32.
    let status = unsafe {
        let boot_services = (*system_table.as_ptr()).boot_services;
        if boot_services.is_null() {
            return None;
        }
        let calculate_crc32 = (*boot_services).calculate_crc32;
        calculate_crc32(data.as_ptr().cast::<c_void>(), data.len(), &mut out)
    };

    (status == Status::SUCCESS).then_some(out)
}
