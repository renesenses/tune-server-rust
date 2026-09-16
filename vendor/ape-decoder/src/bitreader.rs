/// Bit array reader for Monkey's Audio compressed frame data.
///
/// Converts raw frame bytes into a u32 word array (little-endian) and provides
/// bit-level extraction methods used by the range coder and entropy decoder.

const POWERS_OF_TWO_MINUS_ONE: [u32; 33] = [
    0, 1, 3, 7, 15, 31, 63, 127, 255, 511, 1023, 2047, 4095, 8191, 16383, 32767, 65535, 131071,
    262143, 524287, 1048575, 2097151, 4194303, 8388607, 16777215, 33554431, 67108863, 134217727,
    268435455, 536870911, 1073741823, 2147483647, 4294967295,
];

pub struct BitReader {
    words: Vec<u32>,
    bit_index: u32,
}

impl BitReader {
    /// Create a `BitReader` from raw frame bytes, converting to little-endian u32 words.
    ///
    /// `skip_bits` sets the initial bit index (typically 0).
    pub fn from_frame_bytes(raw: &[u8], skip_bits: u32) -> Self {
        let mut words: Vec<u32> = raw
            .chunks(4)
            .map(|chunk| {
                let mut buf = [0u8; 4];
                buf[..chunk.len()].copy_from_slice(chunk);
                u32::from_le_bytes(buf)
            })
            .collect();
        // Append sentinel words so out-of-bounds reads return 0 instead of panicking.
        // This handles malformed files that cause the decoder to read past frame data.
        words.extend_from_slice(&[0; 4]);
        BitReader {
            words,
            bit_index: skip_bits,
        }
    }

    /// Extract one byte from the u32 word array at the current bit position.
    ///
    /// Bytes are read in big-endian order within each 32-bit word (MSB first),
    /// which corresponds to reversed file-byte order due to LE u32 interpretation.
    #[inline]
    pub fn decode_byte(&mut self) -> u32 {
        let word_idx = (self.bit_index >> 5) as usize;
        let bit_off = self.bit_index & 31;
        self.bit_index = self.bit_index.wrapping_add(8);
        let word = self.words.get(word_idx).copied().unwrap_or(0);
        (word >> (24 - bit_off)) & 0xFF
    }

    /// Read `n` raw bits from the bit array, handling word boundary splits.
    ///
    /// Uses the `POWERS_OF_TWO_MINUS_ONE` lookup table for masking.
    #[inline]
    pub fn decode_value_x_bits(&mut self, n: u32) -> u32 {
        let left_bits = 32 - (self.bit_index & 31);
        let word_idx = (self.bit_index >> 5) as usize;
        self.bit_index = self.bit_index.wrapping_add(n);

        let w0 = self.words.get(word_idx).copied().unwrap_or(0);
        if left_bits >= n {
            // Value fits within current word
            (w0 & POWERS_OF_TWO_MINUS_ONE[left_bits as usize]) >> (left_bits - n)
        } else {
            // Split across two words
            let right_bits = n - left_bits;
            let left_value = (w0 & POWERS_OF_TWO_MINUS_ONE[left_bits as usize]) << right_bits;
            let w1 = self.words.get(word_idx + 1).copied().unwrap_or(0);
            let right_value = w1 >> (32 - right_bits);
            left_value | right_value
        }
    }

    /// Advance the bit index to the next byte boundary.
    #[inline]
    pub fn advance_to_byte_boundary(&mut self) {
        let remainder = self.bit_index % 8;
        if remainder != 0 {
            self.bit_index += 8 - remainder;
        }
    }

    /// Advance the bit index by `n` bits without reading.
    #[inline]
    pub fn advance(&mut self, n: u32) {
        self.bit_index = self.bit_index.wrapping_add(n);
    }

    /// Return the current bit index.
    #[inline]
    #[allow(dead_code)]
    pub fn bit_index(&self) -> u32 {
        self.bit_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_byte_order_within_u32() {
        // File bytes [0xAB, 0xCD, 0xEF, 0x12] become LE u32 = 0x12EFCDAB
        // DecodeByte reads MSB first: 0x12, 0xEF, 0xCD, 0xAB
        let mut br = BitReader::from_frame_bytes(&[0xAB, 0xCD, 0xEF, 0x12], 0);
        assert_eq!(br.decode_byte(), 0x12); // file byte 3
        assert_eq!(br.decode_byte(), 0xEF); // file byte 2
        assert_eq!(br.decode_byte(), 0xCD); // file byte 1
        assert_eq!(br.decode_byte(), 0xAB); // file byte 0
    }

    #[test]
    fn decode_byte_across_words() {
        let mut br =
            BitReader::from_frame_bytes(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08], 0);
        // First word: [01,02,03,04] → LE u32 = 0x04030201 → bytes: 04, 03, 02, 01
        assert_eq!(br.decode_byte(), 0x04);
        assert_eq!(br.decode_byte(), 0x03);
        assert_eq!(br.decode_byte(), 0x02);
        assert_eq!(br.decode_byte(), 0x01);
        // Second word: [05,06,07,08] → LE u32 = 0x08070605 → bytes: 08, 07, 06, 05
        assert_eq!(br.decode_byte(), 0x08);
        assert_eq!(br.decode_byte(), 0x07);
    }

    #[test]
    fn decode_value_x_bits_within_word() {
        // u32 = 0x04030201, read 32 bits at once
        let mut br = BitReader::from_frame_bytes(&[0x01, 0x02, 0x03, 0x04], 0);
        let val = br.decode_value_x_bits(32);
        assert_eq!(val, 0x04030201);
    }

    #[test]
    fn decode_value_x_bits_across_boundary() {
        // File: [0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44]
        // Word 0 (LE): 0xDDCCBBAA → MSB-first bytes: DD, CC, BB, AA
        // Word 1 (LE): 0x44332211 → MSB-first bytes: 44, 33, 22, 11
        let mut br =
            BitReader::from_frame_bytes(&[0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44], 0);
        // Skip 24 bits → past DD, CC, BB → positioned at AA
        br.advance(24);
        // Read 16 bits: 8 from word 0 (AA) + 8 from word 1 MSB (44)
        let val = br.decode_value_x_bits(16);
        assert_eq!(val, 0xAA44);
    }

    #[test]
    fn advance_to_byte_boundary() {
        let mut br = BitReader::from_frame_bytes(&[0x00; 8], 0);
        br.advance(3); // bit_index = 3
        br.advance_to_byte_boundary();
        assert_eq!(br.bit_index(), 8);

        br.advance_to_byte_boundary(); // already aligned
        assert_eq!(br.bit_index(), 8);

        br.advance(1); // bit_index = 9
        br.advance_to_byte_boundary();
        assert_eq!(br.bit_index(), 16);
    }

    #[test]
    fn skip_bits_initial() {
        let mut br = BitReader::from_frame_bytes(&[0x01, 0x02, 0x03, 0x04], 8);
        // Skip first byte (bit_index starts at 8), read second byte
        assert_eq!(br.decode_byte(), 0x03); // second byte of MSB-first order
    }
}
