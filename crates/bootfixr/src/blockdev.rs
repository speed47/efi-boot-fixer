//! `gptcore::BlockDevice` over `EFI_BLOCK_IO_PROTOCOL`.

use gptcore::disk::{BlockDevice, IoError};
use uefi::boot::ScopedProtocol;
use uefi::proto::media::block::BlockIO;

pub struct UefiDisk {
    io: ScopedProtocol<BlockIO>,
    media_id: u32,
    block_size: u32,
    last_block: u64,
    read_only: bool,
}

impl UefiDisk {
    pub fn new(io: ScopedProtocol<BlockIO>) -> Result<Self, &'static str> {
        let media = io.media();
        if !media.is_media_present() {
            return Err("no media present");
        }
        let block_size = media.block_size();
        // is_multiple_of() needs rustc 1.87; MSRV is 1.85, so keep the older
        // spelling and silence the newer clippy lint that flags it.
        #[allow(unknown_lints, clippy::manual_is_multiple_of)]
        if block_size < 512 || block_size % 512 != 0 || block_size > 65536 {
            return Err("implausible block size");
        }
        // Buffers come from the global allocator, which is backed by
        // AllocatePool and therefore 8-byte aligned. A device demanding
        // more than that would need bounce buffers we do not implement,
        // so refuse rather than hand it a misaligned pointer.
        if media.io_align() > 8 {
            return Err("device requires stricter buffer alignment than we provide");
        }
        Ok(UefiDisk {
            media_id: media.media_id(),
            block_size,
            last_block: media.last_block(),
            read_only: media.is_read_only(),
            io,
        })
    }
}

impl BlockDevice for UefiDisk {
    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn last_block(&self) -> u64 {
        self.last_block
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), IoError> {
        #[allow(unknown_lints, clippy::manual_is_multiple_of)]
        if buf.len() % self.block_size as usize != 0 {
            return Err(IoError::Unaligned);
        }
        self.io.read_blocks(self.media_id, lba, buf).map_err(|_| IoError::DeviceError)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), IoError> {
        if self.read_only {
            return Err(IoError::ReadOnly);
        }
        #[allow(unknown_lints, clippy::manual_is_multiple_of)]
        if buf.len() % self.block_size as usize != 0 {
            return Err(IoError::Unaligned);
        }
        self.io.write_blocks(self.media_id, lba, buf).map_err(|_| IoError::DeviceError)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.io.flush_blocks().map_err(|_| IoError::DeviceError)
    }
}
