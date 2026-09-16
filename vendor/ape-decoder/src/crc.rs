/// Compute the APE-specific CRC over PCM output bytes.
///
/// This is standard CRC-32 (IEEE) followed by a right-shift by 1 bit,
/// producing a 31-bit value that matches the stored CRC in APE frames.
pub fn ape_crc(data: &[u8]) -> u32 {
    crc32fast::hash(data) >> 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ape_crc_empty() {
        // CRC-32 of empty data is 0x00000000, >> 1 = 0
        assert_eq!(ape_crc(&[]), 0);
    }

    #[test]
    fn test_ape_crc_known() {
        // CRC-32 of "123456789" is 0xCBF43926
        let crc = crc32fast::hash(b"123456789");
        assert_eq!(crc, 0xCBF43926);
        // APE CRC is >> 1
        assert_eq!(ape_crc(b"123456789"), 0xCBF43926 >> 1);
    }

    #[test]
    fn test_ape_crc_zeros() {
        let data = vec![0u8; 1024];
        let crc = ape_crc(&data);
        // Just verify it produces a deterministic non-panic result
        assert!(crc <= 0x7FFFFFFF); // 31-bit value
    }
}
