//! What the firmware says about Secure Boot, read and never written.
//!
//! Five one-byte variables decide the whole story — `SecureBoot`,
//! `SetupMode`, `AuditMode`, `DeployedMode`, `VendorKeys` — and four
//! databases hold the keys themselves. The databases are read for their
//! *size only*: `dbx` on a machine that has taken a few years of revocation
//! updates runs to tens of kilobytes, none of which means anything to
//! somebody diagnosing a boot failure, and pulling it into memory to count
//! its bytes would be the largest allocation this program ever makes.
//!
//! Nothing here changes anything, and nothing here can. Enrolling or
//! clearing a platform key is a decision with consequences this tool has no
//! way to explain on a screen with no keyboard, and the whole reason the
//! state is worth reporting is that it explains a refusal — a firmware in
//! user mode is why `SetVariable` came back `SECURITY_VIOLATION`, and why a
//! freshly installed GRUB will not load.

use alloc::string::String;
use alloc::vec::Vec;
use uefi::runtime::{self, VariableVendor};
use uefi::{cstr16, CStr16};

/// One of the signature databases, and how big it is.
pub struct Database {
    pub name: &'static str,
    /// Its size in bytes, `None` if it is not present at all, or the
    /// reason it could not be measured.
    pub size: Result<Option<usize>, String>,
}

/// The whole read-only picture.
pub struct State {
    /// `SecureBoot`: 1 while signature verification is being enforced.
    pub secure_boot: Option<u8>,
    /// `SetupMode`: 1 when there is no platform key and anything may be
    /// enrolled.
    pub setup_mode: Option<u8>,
    pub audit_mode: Option<u8>,
    pub deployed_mode: Option<u8>,
    /// `VendorKeys`: 0 once the keys have been modified since manufacture.
    pub vendor_keys: Option<u8>,
    pub databases: Vec<Database>,
}

impl State {
    /// One line for the top of a report: enforcing, off, or not implemented.
    pub fn summary(&self) -> String {
        match (self.secure_boot, self.setup_mode) {
            (None, None) => String::from("not implemented by this firmware"),
            (Some(1), Some(1)) => String::from("enabled, but the firmware is in setup mode"),
            (Some(1), _) => String::from("ENABLED - signatures are being enforced"),
            (Some(_), Some(1)) => String::from("disabled, and the firmware is in setup mode"),
            (Some(_), _) => String::from("disabled"),
            (None, Some(_)) => String::from("no SecureBoot variable, so not enforced"),
        }
    }
}

/// A one-byte variable, or `None` if it is absent or the wrong size.
///
/// Wrong size is treated as absent rather than guessed at, exactly as
/// [`crate::nvram`] does for its `u16`s: these are all defined as a single
/// boolean byte, and a firmware that stored something else is telling us
/// nothing we can repeat honestly.
fn flag(name: &CStr16) -> Option<u8> {
    let (data, _) = runtime::get_variable_boxed(name, &VariableVendor::GLOBAL_VARIABLE).ok()?;
    (data.len() == 1).then(|| data[0])
}

/// Read everything, best effort. Every field can be absent, and on plenty
/// of firmware most of them are.
pub fn read() -> State {
    let mut databases = Vec::new();
    // PK and KEK are global; db, dbx and dbt live in the image security
    // database namespace. Getting that wrong reports every database as
    // missing on a machine that has all of them.
    for (label, name, vendor) in [
        ("PK", cstr16!("PK"), VariableVendor::GLOBAL_VARIABLE),
        ("KEK", cstr16!("KEK"), VariableVendor::GLOBAL_VARIABLE),
        ("db", cstr16!("db"), VariableVendor::IMAGE_SECURITY_DATABASE),
        ("dbx", cstr16!("dbx"), VariableVendor::IMAGE_SECURITY_DATABASE),
        ("dbt", cstr16!("dbt"), VariableVendor::IMAGE_SECURITY_DATABASE),
    ] {
        databases.push(Database { name: label, size: crate::nvram::size_of(name, &vendor) });
    }

    State {
        secure_boot: flag(cstr16!("SecureBoot")),
        setup_mode: flag(cstr16!("SetupMode")),
        audit_mode: flag(cstr16!("AuditMode")),
        deployed_mode: flag(cstr16!("DeployedMode")),
        vendor_keys: flag(cstr16!("VendorKeys")),
        databases,
    }
}
