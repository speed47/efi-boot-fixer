//! GPT parsing, validation and repair planning.
//!
//! `no_std` and free of any UEFI dependency, so the logic that decides
//! what to overwrite on a disk runs identically under firmware and under
//! `cargo test`.
//!
//! The intended flow is:
//!
//! 1. [`repair::analyze`] reads both tables and returns a [`repair::Verdict`].
//! 2. [`repair::plan`] turns a repairable verdict into an ordered
//!    [`repair::RepairPlan`].
//! 3. The caller shows the plan to a human and only then calls
//!    [`repair::apply`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod crc;
pub mod disk;
pub mod entry;
pub mod guid;
pub mod header;
pub mod layout;
pub mod mbr;
pub mod repair;
pub mod report;

pub use crc::{Crc32, SoftCrc32};
pub use disk::{BlockDevice, IoError};
pub use entry::PartitionEntry;
pub use guid::Guid;
pub use header::{Defect, GptHeader};
pub use repair::{analyze, apply, plan, Analysis, RepairPlan, Step, TableView, Verdict};
