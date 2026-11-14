//! CRC-32C (Castagnoli), for the control header's layout checksum.

pub fn crc32c(chunks: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for chunk in chunks {
        for &b in *chunk {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    #[test]
    fn known_vector() {
        // The standard check value for CRC-32C.
        assert_eq!(super::crc32c(&[b"123456789"]), 0xE306_9283);
    }
}
