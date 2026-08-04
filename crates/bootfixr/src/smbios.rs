//! Finding the SMBIOS structure table and copying it out.
//!
//! The parsing is [`gptcore::smbios`], where it can be tested on a host
//! against a synthetic table. This module is only the part that cannot be:
//! two raw pointers handed over by the firmware, and the anchors that say
//! whether they are what they claim to be.
//!
//! The table is **copied into memory we own** before anything looks at it.
//! Parsing it in place would mean every bounds check in `gptcore` guarding a
//! read from firmware-owned memory through a raw pointer — the one shape of
//! bug that does not produce a wrong line in a report but a machine that
//! stops. One copy, bounded by the length the entry point declares and by
//! [`MAX_TABLE`], and everything after it is a slice.

use alloc::vec::Vec;
use uefi::table::cfg;

/// The largest structure table worth copying.
///
/// Real ones are a few kilobytes. This is not a limit anybody will meet; it
/// is a bound on what a garbled entry point can talk us into allocating,
/// and an allocation the firmware refuses is not a missing section in the
/// report but the `no_std` allocation error handler.
const MAX_TABLE: usize = 1 << 20;

/// The structure table's bytes, if this firmware publishes one.
///
/// Prefers the 64-bit entry point, since a machine that publishes both is
/// describing the same table twice and the 3.0 one is the one it maintains.
pub fn table() -> Option<Vec<u8>> {
    let (v3, v1) = uefi::system::with_config_table(|entries| {
        let find = |guid| entries.iter().find(|e| e.guid == guid).map(|e| e.address);
        (find(cfg::SMBIOS3_GUID), find(cfg::SMBIOS_GUID))
    });
    v3.and_then(|p| read_v3(p.cast())).or_else(|| v1.and_then(|p| read_v1(p.cast())))
}

/// Copy `len` bytes from firmware-owned memory.
///
/// # Safety
///
/// `at` must be the address the firmware published for a table it says is
/// `len` bytes long, and both must have come out of a validated entry point.
unsafe fn copy(at: *const u8, len: usize) -> Option<Vec<u8>> {
    if at.is_null() || len == 0 || len > MAX_TABLE {
        return None;
    }
    // SAFETY: the caller has validated the anchor of the entry point that
    // named this address and this length, and the firmware owns the region
    // for the life of the boot. The copy is immediate; nothing holds the
    // slice afterwards.
    Some(unsafe { core::slice::from_raw_parts(at, len) }.to_vec())
}

/// The entry point itself, which is small and fixed-size.
///
/// # Safety
///
/// `at` must be an address the firmware published in its configuration
/// table as an SMBIOS entry point.
unsafe fn entry_point(at: *const u8, len: usize) -> Option<Vec<u8>> {
    if at.is_null() {
        return None;
    }
    // SAFETY: an entry point structure is at least this long by definition
    // of the anchors we are about to check, and the configuration table is
    // the firmware's own statement that something lives here.
    Some(unsafe { core::slice::from_raw_parts(at, len) }.to_vec())
}

/// SMBIOS 3.0: `_SM3_`, a 64-bit table address, a 32-bit maximum size.
///
/// **Maximum**, not exact: 3.0 dropped the exact length that 2.1 carried at
/// offset 0x16, and a table's real end is the type-127 structure inside it.
/// So this copies as much as the firmware says the table may be, which is
/// what every other consumer of SMBIOS does for want of anything better,
/// and is the one read in this program not bounded by a length something
/// else has agreed to. Firmware that rounded that maximum up past its own
/// allocation would have it read anyway. The alternative — walking the
/// structures in firmware memory to find the end before copying — moves the
/// same unchecked reads earlier and does them one at a time instead of once.
fn read_v3(at: *const u8) -> Option<Vec<u8>> {
    // SAFETY: see `entry_point`.
    let ep = unsafe { entry_point(at, 0x18) }?;
    if ep.get(..5)? != b"_SM3_" {
        return None;
    }
    let len = u32::from_le_bytes(ep.get(0x0C..0x10)?.try_into().ok()?) as usize;
    let addr = u64::from_le_bytes(ep.get(0x10..0x18)?.try_into().ok()?);
    // SAFETY: the anchor above is the firmware's own statement that this is
    // an SMBIOS 3 entry point, so its address and length fields are the
    // table's.
    unsafe { copy(addr as usize as *const u8, len) }
}

/// SMBIOS 2.1: `_SM_`, a 32-bit table address, a 16-bit length.
fn read_v1(at: *const u8) -> Option<Vec<u8>> {
    // SAFETY: see `entry_point`.
    let ep = unsafe { entry_point(at, 0x1F) }?;
    if ep.get(..4)? != b"_SM_" {
        return None;
    }
    let len = u16::from_le_bytes(ep.get(0x16..0x18)?.try_into().ok()?) as usize;
    let addr = u32::from_le_bytes(ep.get(0x18..0x1C)?.try_into().ok()?);
    // SAFETY: as above, for the 2.1 anchor.
    unsafe { copy(addr as usize as *const u8, len) }
}
