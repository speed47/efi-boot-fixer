//! Which disk did this image come from?
//!
//! `LoadedImage->DeviceHandle` is the *partition* the image came from (the
//! ESP), not the disk. To attribute it to a whole device we compare device
//! paths: a disk carries the boot volume if its path is a prefix of the
//! boot volume's path.
//!
//! This used to be an exclusion. It is now only a label. The tool is meant
//! to be copied onto the Deck's own ESP and run from there, so the disk it
//! booted from is usually the disk that needs repairing — refusing to touch
//! it would defeat the point. Knowing which one it is still matters, both
//! because the operator should be told what they are pointing at and
//! because writing to that disk tears down the filesystem driver serving
//! the ESP (see `esp::Volume`).

use alloc::boxed::Box;
use uefi::boot;
use uefi::proto::device_path::{DevicePath, DevicePathNode};
use uefi::proto::loaded_image::LoadedImage;

pub struct BootDevice {
    path: Option<Box<DevicePath>>,
}

impl BootDevice {
    /// Resolve the device path of the volume this image was loaded from.
    pub fn resolve() -> Self {
        BootDevice { path: Self::lookup() }
    }

    fn lookup() -> Option<Box<DevicePath>> {
        let image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle()).ok()?;
        let device = image.device()?;
        let path = boot::open_protocol_exclusive::<DevicePath>(device).ok()?;
        Some(path.to_boxed())
    }

    pub fn is_known(&self) -> bool {
        self.path.is_some()
    }

    pub fn path(&self) -> Option<&DevicePath> {
        self.path.as_deref()
    }

    /// True if `disk` is the device we booted from, or carries it.
    ///
    /// An unknown boot device answers `false` for every disk: nothing gets
    /// the label, and the menu says so rather than claiming a disk is not
    /// the boot disk when we simply cannot tell.
    pub fn covers(&self, disk: &DevicePath) -> bool {
        let Some(boot) = self.path.as_deref() else {
            return false;
        };
        is_prefix_of(disk, boot)
    }
}

fn real_nodes(path: &DevicePath) -> impl Iterator<Item = &DevicePathNode> {
    // End-of-path markers would otherwise fail to match the boot path's
    // continuing nodes and defeat the prefix test.
    path.node_iter().filter(|n| !n.is_end_entire())
}

/// True if every node of `prefix` matches the corresponding node of `full`.
fn is_prefix_of(prefix: &DevicePath, full: &DevicePath) -> bool {
    let mut full_nodes = real_nodes(full);
    let mut matched = 0usize;
    for node in real_nodes(prefix) {
        match full_nodes.next() {
            Some(candidate) if candidate == node => matched += 1,
            _ => return false,
        }
    }
    // An empty path is a prefix of everything; that is not a match we want
    // to act on.
    matched > 0
}
