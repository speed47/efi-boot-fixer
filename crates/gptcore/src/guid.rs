//! GUIDs as they appear on disk: 16 bytes, mixed-endian.
//!
//! The first three fields are stored little-endian, the last two as raw
//! bytes. That is why the byte order in a hexdump does not match the
//! textual form.

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const ZERO: Guid = Guid([0u8; 16]);

    /// Build from the canonical textual grouping, e.g.
    /// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` becomes
    /// `from_fields(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, ...])`.
    pub const fn from_fields(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Self {
        let a = d1.to_le_bytes();
        let b = d2.to_le_bytes();
        let c = d3.to_le_bytes();
        Guid([
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4],
            d4[5], d4[6], d4[7],
        ])
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 16]
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Read from a buffer. Returns `None` if fewer than 16 bytes remain.
    pub fn read_from(buf: &[u8], offset: usize) -> Option<Self> {
        let slice = buf.get(offset..offset.checked_add(16)?)?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(slice);
        Some(Guid(bytes))
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        let d1 = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let d2 = u16::from_le_bytes([b[4], b[5]]);
        let d3 = u16::from_le_bytes([b[6], b[7]]);
        write!(f, "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-", d1, d2, d3, b[8], b[9])?;
        for byte in &b[10..16] {
            write!(f, "{:02X}", byte)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::string::ToString;

    #[test]
    fn esp_guid_round_trips_through_text() {
        let esp = Guid::from_fields(
            0xC12A_7328,
            0xF81F,
            0x11D2,
            [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
        );
        assert_eq!(esp.to_string(), "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
    }

    #[test]
    fn on_disk_byte_order_is_mixed_endian() {
        // Exactly the bytes sgdisk writes for the ESP type GUID.
        let esp = Guid::from_fields(
            0xC12A_7328,
            0xF81F,
            0x11D2,
            [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
        );
        assert_eq!(
            esp.as_bytes(),
            &[
                0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
                0xC9, 0x3B
            ]
        );
    }

    #[test]
    fn zero_guid_marks_an_unused_entry() {
        assert!(Guid::ZERO.is_zero());
        assert!(!Guid::from_fields(1, 0, 0, [0; 8]).is_zero());
    }
}
