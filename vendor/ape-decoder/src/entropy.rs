/// Entropy decoder for Monkey's Audio (version >= 3950).
///
/// Implements `DecodeValueRange` -- the main entry point for decoding a single
/// sample's residual value from the range-coded bitstream.
use crate::bitreader::BitReader;
use crate::error::ApeResult;
use crate::range_coder::RangeCoder;

// ---------------------------------------------------------------------------
// K_SUM_MIN_BOUNDARY table (32 entries)
// ---------------------------------------------------------------------------

const K_SUM_MIN_BOUNDARY: [u32; 32] = [
    0,          // [0]
    32,         // [1]
    64,         // [2]
    128,        // [3]
    256,        // [4]
    512,        // [5]
    1024,       // [6]
    2048,       // [7]
    4096,       // [8]
    8192,       // [9]
    16384,      // [10]  <-- initial k=10, k_sum=16384
    32768,      // [11]
    65536,      // [12]
    131072,     // [13]
    262144,     // [14]
    524288,     // [15]
    1048576,    // [16]
    2097152,    // [17]
    4194304,    // [18]
    8388608,    // [19]
    16777216,   // [20]
    33554432,   // [21]
    67108864,   // [22]
    134217728,  // [23]
    268435456,  // [24]
    536870912,  // [25]
    1073741824, // [26]
    2147483648, // [27]
    0,          // [28]  zero sentinel
    0,          // [29]
    0,          // [30]
    0,          // [31]
];

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BOTTOM_VALUE: u32 = 0x0080_0000;

// ---------------------------------------------------------------------------
// EntropyState
// ---------------------------------------------------------------------------

/// Per-channel entropy decoder state tracking the adaptive k parameter.
pub struct EntropyState {
    pub k: u32,
    pub k_sum: u32,
}

impl EntropyState {
    pub fn new() -> Self {
        let mut state = EntropyState { k: 0, k_sum: 0 };
        state.flush();
        state
    }

    /// Reset state at the start of each frame.
    pub fn flush(&mut self) {
        self.k = 10;
        self.k_sum = (1u32 << self.k).wrapping_mul(16); // 1024 * 16 = 16384
    }

    /// Select the entropy syntax from the file header. Keep the existing
    /// public method below as the modern-format API.
    #[inline]
    pub fn decode_value_range_for_version(
        &mut self,
        rc: &mut RangeCoder,
        br: &mut BitReader,
        version: i32,
    ) -> ApeResult<i64> {
        if version >= 3990 {
            return self.decode_value_range(rc, br);
        }
        self.decode_legacy_value(rc, br)
    }

    fn decode_legacy_value(&mut self, rc: &mut RangeCoder, br: &mut BitReader) -> ApeResult<i64> {
        let symbol = rc.decode_legacy_overflow(br)?;
        let (quotient, bits) = match symbol {
            63 => (0, rc.range_decode_fast_with_update(br, 5)?),
            _ => (symbol, self.k.saturating_sub(1)),
        };
        if bits > 31 {
            return Err(crate::error::ApeError::DecodingError(
                "legacy Rice width exceeds 31 bits",
            ));
        }
        // Low bits precede high bits in this syntax; each arithmetic-coder
        // operation is at most 16 bits wide.
        let low_bits = bits.min(16);
        let mut remainder = rc.range_decode_fast_with_update(br, low_bits)?;
        if bits > 16 {
            remainder |= rc.range_decode_fast_with_update(br, bits - 16)? << 16;
        }
        let value = remainder.wrapping_add(quotient.wrapping_shl(bits));
        self.k_sum = self
            .k_sum
            .wrapping_add((value.wrapping_add(1)) >> 1)
            .wrapping_sub((self.k_sum.wrapping_add(16)) >> 5);
        if self.k > 0 && self.k_sum < K_SUM_MIN_BOUNDARY[self.k as usize] {
            self.k -= 1;
        } else if self.k < 24 && self.k_sum >= K_SUM_MIN_BOUNDARY[self.k as usize + 1] {
            self.k += 1;
        }
        if value & 1 != 0 {
            Ok((value as i64 >> 1) + 1)
        } else {
            Ok(-(value as i64 >> 1))
        }
    }

    /// Decode a single sample residual value from the range-coded bitstream.
    ///
    /// Returns the signed residual as i64. The value is decoded from an unsigned
    /// interleaved representation (0, +1, -1, +2, -2, ...) and converted to
    /// signed form at the end.
    pub fn decode_value_range(
        &mut self,
        rc: &mut RangeCoder,
        br: &mut BitReader,
    ) -> ApeResult<i64> {
        // Step 1: compute pivot value from k_sum
        let mut pivot_value: u32 = (self.k_sum / 32).max(1);

        // Step 2: decode the overflow (quotient)
        let overflow: u32 = rc.decode_overflow(br, &mut pivot_value)?;

        // Step 3: decode the base (remainder) from uniform distribution [0, pivot_value)
        let base: u32;

        if pivot_value >= (1 << 16) {
            // Large pivot: split into two smaller range-coded values
            let mut pivot_value_bits: u32 = 0;
            let mut tmp = pivot_value;
            while tmp > 0 {
                pivot_value_bits += 1;
                tmp >>= 1;
            }

            let shift = if pivot_value_bits >= 16 {
                pivot_value_bits - 16
            } else {
                0
            };
            let split_factor: u32 = 1u32 << shift;

            let pivot_value_a: u32 = (pivot_value / split_factor).wrapping_add(1);
            let pivot_value_b: u32 = split_factor;

            // Decode upper portion
            rc.normalize(br);
            rc.range /= pivot_value_a;
            let base_a = rc.low / rc.range;
            rc.low %= rc.range;

            // Decode lower portion
            rc.normalize(br);
            rc.range /= pivot_value_b;
            let base_b = rc.low / rc.range;
            rc.low %= rc.range;

            base = base_a.wrapping_mul(split_factor).wrapping_add(base_b);
        } else {
            // Small pivot: single range-coded value with inline normalization
            while rc.range <= BOTTOM_VALUE {
                rc.buffer = rc.buffer.wrapping_shl(8) | br.decode_byte();
                rc.low = rc.low.wrapping_shl(8) | ((rc.buffer >> 1) & 0xFF);
                rc.range = rc.range.wrapping_shl(8);

                if rc.range == 0 {
                    return Ok(0); // end-of-life
                }
            }

            rc.range /= pivot_value;
            base = rc.low / rc.range;
            rc.low %= rc.range;
        }

        // Step 4: combine overflow and base into the unsigned interleaved value
        let value: i64 = (base as i64) + (overflow as i64) * (pivot_value as i64);

        // Step 5: update k_sum
        //   (value + 1) / 2 computes the magnitude of the signed result
        self.k_sum = self
            .k_sum
            .wrapping_add(((value + 1) / 2) as u32)
            .wrapping_sub((self.k_sum.wrapping_add(16)) >> 5);

        // Step 6: update k (parameter for the next value)
        if self.k_sum < K_SUM_MIN_BOUNDARY[self.k as usize] {
            self.k -= 1;
        } else if K_SUM_MIN_BOUNDARY[(self.k + 1) as usize] != 0
            && self.k_sum >= K_SUM_MIN_BOUNDARY[(self.k + 1) as usize]
        {
            self.k += 1;
        }

        // Step 7: convert from unsigned interleaved to signed
        //   odd  values -> positive: (value >> 1) + 1
        //   even values -> non-positive: -(value >> 1)
        if (value & 1) != 0 {
            Ok((value >> 1) + 1)
        } else {
            Ok(-(value >> 1))
        }
    }
}
