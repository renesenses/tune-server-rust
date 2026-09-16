/// Range coder for Monkey's Audio (version >= 3950).
///
/// All u32 arithmetic uses wrapping operations to match C++ unsigned overflow semantics.
use crate::bitreader::BitReader;
use crate::error::{ApeError, ApeResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CODE_BITS: u32 = 32;
const TOP_VALUE: u32 = 1u32 << (CODE_BITS - 1); // 0x80000000
const EXTRA_BITS: u32 = (CODE_BITS - 2) % 8 + 1; // 7
const BOTTOM_VALUE: u32 = TOP_VALUE >> 8; // 0x00800000
const RANGE_OVERFLOW_SHIFT: u32 = 16;
const MODEL_ELEMENTS: u32 = 64;
const OVERFLOW_SIGNAL: u32 = 1;
const OVERFLOW_PIVOT_VALUE: u32 = 32768;

// ---------------------------------------------------------------------------
// Probability tables (version >= 3990 only)
// ---------------------------------------------------------------------------

const RANGE_TOTAL_2: [u32; 65] = [
    0, 19578, 36160, 48417, 56323, 60899, 63265, 64435, 64971, 65232, 65351, 65416, 65447, 65466,
    65476, 65482, 65485, 65488, 65490, 65491, 65492, 65493, 65494, 65495, 65496, 65497, 65498,
    65499, 65500, 65501, 65502, 65503, 65504, 65505, 65506, 65507, 65508, 65509, 65510, 65511,
    65512, 65513, 65514, 65515, 65516, 65517, 65518, 65519, 65520, 65521, 65522, 65523, 65524,
    65525, 65526, 65527, 65528, 65529, 65530, 65531, 65532, 65533, 65534, 65535, 65536,
];

const RANGE_WIDTH_2: [u32; 64] = [
    19578, 16582, 12257, 7906, 4576, 2366, 1170, 536, 261, 119, 65, 31, 19, 10, 6, 3, 3, 2, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

// ---------------------------------------------------------------------------
// Overflow lookup table (built once at init)
// ---------------------------------------------------------------------------

/// Build the 65536-entry overflow lookup table from RANGE_TOTAL_2.
fn build_overflow_table() -> Box<[u8; 65536]> {
    let mut table = Box::new([0u8; 65536]);
    let mut overflow: u8 = 0;
    for z in 0..65536u32 {
        if z >= RANGE_TOTAL_2[(overflow as usize) + 1] {
            overflow += 1;
        }
        table[z as usize] = overflow;
    }
    table
}

// ---------------------------------------------------------------------------
// RangeCoder
// ---------------------------------------------------------------------------

pub struct RangeCoder {
    pub low: u32,
    pub range: u32,
    pub buffer: u32,
    overflow_table: Box<[u8; 65536]>,
}

impl RangeCoder {
    /// Create a new range coder with zeroed state and precomputed overflow table.
    pub fn new() -> Self {
        RangeCoder {
            low: 0,
            range: 0,
            buffer: 0,
            overflow_table: build_overflow_table(),
        }
    }

    /// Initialize the range coder from the bit array at the start of a frame.
    ///
    /// Advances to byte boundary, skips the mandatory dummy byte, reads the seed
    /// byte, and sets initial range/low values.
    pub fn flush_bit_array(&mut self, br: &mut BitReader) {
        br.advance_to_byte_boundary();
        br.decode_value_x_bits(8); // skip dummy byte
        self.buffer = br.decode_value_x_bits(8); // seed byte
        self.low = self.buffer >> (8 - EXTRA_BITS); // buffer >> 1
        self.range = 1u32 << EXTRA_BITS; // 128
    }

    /// Range normalization loop.
    ///
    /// Feeds bytes from the bit reader into the range coder state until
    /// `range > BOTTOM_VALUE`.
    #[inline]
    pub fn normalize(&mut self, br: &mut BitReader) {
        while self.range <= BOTTOM_VALUE {
            self.buffer = self.buffer.wrapping_shl(8) | br.decode_byte();
            self.low = self.low.wrapping_shl(8) | ((self.buffer >> 1) & 0xFF);
            self.range = self.range.wrapping_shl(8);
        }
    }

    /// Decode a value from a uniform distribution of size `1 << shift`.
    ///
    /// Does NOT update `low` -- the caller must do so (used by `decode_overflow`).
    /// Returns 0 if range wraps to zero (end-of-life sentinel).
    #[inline]
    pub fn range_decode_fast(&mut self, br: &mut BitReader, shift: u32) -> u32 {
        // Normalize with end-of-life check
        while self.range <= BOTTOM_VALUE {
            self.buffer = self.buffer.wrapping_shl(8) | br.decode_byte();
            self.low = self.low.wrapping_shl(8) | ((self.buffer >> 1) & 0xFF);
            self.range = self.range.wrapping_shl(8);

            if self.range == 0 {
                return 0;
            }
        }

        self.range >>= shift;
        self.low / self.range
    }

    /// Decode a value from a uniform distribution of size `1 << shift`,
    /// updating `low` to `low % range` afterward.
    ///
    /// Returns an error if range becomes zero (corrupt input).
    #[inline]
    pub fn range_decode_fast_with_update(
        &mut self,
        br: &mut BitReader,
        shift: u32,
    ) -> ApeResult<u32> {
        // Normalize with corruption check
        while self.range <= BOTTOM_VALUE {
            if self.range == 0 {
                return Err(ApeError::DecodingError(
                    "range coder: range is zero during normalization",
                ));
            }
            self.buffer = self.buffer.wrapping_shl(8) | br.decode_byte();
            self.low = self.low.wrapping_shl(8) | ((self.buffer >> 1) & 0xFF);
            self.range = self.range.wrapping_shl(8);
        }

        self.range >>= shift;

        if self.range == 0 {
            return Err(ApeError::DecodingError(
                "range coder: range is zero after shift",
            ));
        }

        let result = self.low / self.range;
        self.low %= self.range;
        Ok(result)
    }

    /// Decode the overflow (quotient) portion of a value using the model
    /// probability tables and the overflow lookup table.
    ///
    /// `pivot_value` may be mutated to `OVERFLOW_PIVOT_VALUE` if the overflow
    /// signaling mechanism fires.
    pub fn decode_overflow(&mut self, br: &mut BitReader, pivot_value: &mut u32) -> ApeResult<u32> {
        // Step 1: decode from uniform distribution of size 65536
        let range_total = self.range_decode_fast(br, RANGE_OVERFLOW_SHIFT);
        if range_total >= 65536 {
            return Err(ApeError::DecodingError(
                "range coder: overflow range_total out of bounds",
            ));
        }

        // Step 2: look up symbol from the 65536-entry table
        let mut overflow = self.overflow_table[range_total as usize] as u32;

        // Step 3: update range coder state using model probabilities
        // low -= range * RANGE_TOTAL_2[overflow] (wrapping)
        self.low = self
            .low
            .wrapping_sub(self.range.wrapping_mul(RANGE_TOTAL_2[overflow as usize]));
        // range = range * RANGE_WIDTH_2[overflow] (wrapping)
        self.range = self.range.wrapping_mul(RANGE_WIDTH_2[overflow as usize]);

        // Step 4: handle large overflow (symbol == 63)
        if overflow == (MODEL_ELEMENTS - 1) {
            // Read two 16-bit halves to form a 32-bit overflow value
            overflow = self.range_decode_fast_with_update(br, 16)?;
            overflow <<= 16;
            overflow |= self.range_decode_fast_with_update(br, 16)?;

            // Detect overflow signaling: recurse with forced pivot
            if overflow == OVERFLOW_SIGNAL {
                *pivot_value = OVERFLOW_PIVOT_VALUE;
                return self.decode_overflow(br, pivot_value);
            }
        }

        Ok(overflow)
    }

    /// Decode the quotient symbol using the pre-3.99 bitstream model.
    /// The cumulative frequencies are format constants. Symbols 21..63
    /// occupy one interval each; symbol 63 escapes to an explicit Rice width.
    pub fn decode_legacy_overflow(&mut self, br: &mut BitReader) -> ApeResult<u32> {
        const CUMULATIVE: [u32; 22] = [
            0, 14824, 28224, 39348, 47855, 53994, 58171, 60926, 62682, 63786, 64463, 64878, 65126,
            65276, 65365, 65419, 65450, 65469, 65480, 65487, 65491, 65493,
        ];
        let quantile = self.range_decode_fast(br, 16);
        if self.range == 0 || quantile >= 65536 {
            return Err(ApeError::DecodingError(
                "legacy range coder: invalid interval",
            ));
        }
        let (symbol, start, width) = if quantile >= CUMULATIVE[21] {
            (quantile - CUMULATIVE[21] + 21, quantile, 1)
        } else {
            let index = CUMULATIVE.partition_point(|&start| start <= quantile) - 1;
            (
                index as u32,
                CUMULATIVE[index],
                CUMULATIVE[index + 1] - CUMULATIVE[index],
            )
        };
        self.low = self.low.wrapping_sub(self.range.wrapping_mul(start));
        self.range = self.range.wrapping_mul(width);
        Ok(symbol)
    }

    /// Consume remaining normalization bytes at the end of a frame.
    pub fn finalize(&mut self, br: &mut BitReader) {
        while self.range <= BOTTOM_VALUE {
            br.advance(8);
            self.range = self.range.wrapping_shl(8);
            if self.range == 0 {
                return;
            }
        }
    }
}
