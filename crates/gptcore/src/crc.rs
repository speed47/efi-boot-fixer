//! CRC-32 as an injected dependency.
//!
//! On the Deck the implementation is the firmware's
//! `gBS->CalculateCrc32`, so the values we write are computed by the same
//! code that will later validate them. [`SoftCrc32`] exists so host tests
//! and the loopback tooling can run the identical logic off-target.

/// CRC-32/ISO-HDLC (the polynomial UEFI specifies for GPT).
pub trait Crc32 {
    fn crc32(&self, data: &[u8]) -> u32;
}

/// Portable table-free implementation, for host tests and tooling.
#[derive(Clone, Copy, Debug, Default)]
pub struct SoftCrc32;

impl Crc32 for SoftCrc32 {
    fn crc32(&self, data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= byte as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
}

impl<T: Crc32 + ?Sized> Crc32 for &T {
    fn crc32(&self, data: &[u8]) -> u32 {
        (**self).crc32(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vectors() {
        assert_eq!(SoftCrc32.crc32(b""), 0x0000_0000);
        assert_eq!(SoftCrc32.crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(SoftCrc32.crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn zero_filled_block_has_the_expected_crc() {
        // Cross-checked against zlib.crc32(bytes(512)).
        assert_eq!(SoftCrc32.crc32(&[0u8; 512]), 0xB2AA_7578);
    }
}
