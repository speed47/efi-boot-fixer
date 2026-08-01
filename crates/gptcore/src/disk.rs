//! Block I/O abstraction.
//!
//! `gptcore` never talks to firmware. The UEFI application implements this
//! over `EFI_BLOCK_IO_PROTOCOL`; the tests implement it over a file. That
//! is the whole reason the repair logic can be exercised on the host.

use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoError {
    /// The request runs past the end of the device.
    OutOfRange,
    /// The buffer is not a whole number of blocks.
    Unaligned,
    /// The device reported a failure.
    DeviceError,
    /// The device is not writable.
    ReadOnly,
}

impl core::fmt::Display for IoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            IoError::OutOfRange => "request past end of device",
            IoError::Unaligned => "buffer is not a whole number of blocks",
            IoError::DeviceError => "device error",
            IoError::ReadOnly => "device is not writable",
        };
        f.write_str(s)
    }
}

pub trait BlockDevice {
    fn block_size(&self) -> u32;

    /// LBA of the last addressable block, i.e. `Media->LastBlock`. The
    /// device therefore has `last_block + 1` blocks.
    fn last_block(&self) -> u64;

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError>;
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), IoError>;
    fn flush(&mut self) -> Result<(), IoError>;
}

/// Read `count` blocks starting at `lba` into a fresh buffer.
pub fn read_lbas<D: BlockDevice + ?Sized>(
    dev: &mut D,
    lba: u64,
    count: u64,
) -> Result<Vec<u8>, IoError> {
    let block_size = dev.block_size() as u64;
    let len = count.checked_mul(block_size).ok_or(IoError::OutOfRange)?;
    let len = usize::try_from(len).map_err(|_| IoError::OutOfRange)?;

    // Refuse before allocating, so a corrupt block count cannot make us
    // try to reserve an absurd buffer.
    let end = lba.checked_add(count).ok_or(IoError::OutOfRange)?;
    if end > dev.last_block().saturating_add(1) {
        return Err(IoError::OutOfRange);
    }

    let mut buf = alloc::vec![0u8; len];
    dev.read_blocks(lba, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
pub mod testdev {
    //! An in-memory device for unit tests.

    use super::*;

    pub struct MemDisk {
        pub data: Vec<u8>,
        pub block_size: u32,
        pub writable: bool,
        /// Every write recorded in order, for asserting on write ordering.
        pub journal: Vec<(u64, usize)>,
        /// Positions in `journal` at which a flush occurred.
        pub flushes: Vec<usize>,
    }

    impl MemDisk {
        pub fn new(blocks: u64, block_size: u32) -> Self {
            MemDisk {
                data: alloc::vec![0u8; (blocks * block_size as u64) as usize],
                block_size,
                writable: true,
                journal: Vec::new(),
                flushes: Vec::new(),
            }
        }
    }

    impl BlockDevice for MemDisk {
        fn block_size(&self) -> u32 {
            self.block_size
        }

        fn last_block(&self) -> u64 {
            self.data.len() as u64 / self.block_size as u64 - 1
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
            if buf.len() % self.block_size as usize != 0 {
                return Err(IoError::Unaligned);
            }
            let off = (lba * self.block_size as u64) as usize;
            let end = off.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
            if end > self.data.len() {
                return Err(IoError::OutOfRange);
            }
            buf.copy_from_slice(&self.data[off..end]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), IoError> {
            if !self.writable {
                return Err(IoError::ReadOnly);
            }
            if buf.len() % self.block_size as usize != 0 {
                return Err(IoError::Unaligned);
            }
            let off = (lba * self.block_size as u64) as usize;
            let end = off.checked_add(buf.len()).ok_or(IoError::OutOfRange)?;
            if end > self.data.len() {
                return Err(IoError::OutOfRange);
            }
            self.data[off..end].copy_from_slice(buf);
            self.journal.push((lba, buf.len() / self.block_size as usize));
            Ok(())
        }

        fn flush(&mut self) -> Result<(), IoError> {
            self.flushes.push(self.journal.len());
            Ok(())
        }
    }
}
