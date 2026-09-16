/// Predictor stage for the Monkey's Audio decoder.
///
/// Implements:
/// - `ScaledFirstOrderFilter` -- simple IIR filter (multiply=31, shift=5)
/// - `Predictor3950` -- for version >= 3950, bitsPerSample < 32
/// - `Predictor3950_32` -- for version >= 3950, bitsPerSample >= 32
///
/// Reference: `NewPredictor.h`, `NewPredictor.cpp`,
/// `ScaledFirstOrderFilter.h`.
use crate::nn_filter::{
    create_filters_16, create_filters_32, decompress_cascade_16, decompress_cascade_32,
    flush_cascade_16, flush_cascade_32, set_interim_mode_16, set_interim_mode_32, NNFilter16,
    NNFilter32,
};
use crate::roll_buffer::RollBuffer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const WINDOW_BLOCKS: usize = 256;
const PRED_HISTORY: usize = 8;
const M_COUNT: usize = 8;

/// Initial coefficients for ary_ma.
const INITIAL_MA: [i32; M_COUNT] = [360, 317, -109, 98, 0, 0, 0, 0];
const INITIAL_MA_64: [i64; M_COUNT] = [360, 317, -109, 98, 0, 0, 0, 0];

// ---------------------------------------------------------------------------
// ScaledFirstOrderFilter
// ---------------------------------------------------------------------------

/// Simple first-order IIR filter: `y[n] = x[n] + (last * 31) >> 5`.
///
/// Template parameters in C++: `<INTTYPE, MULTIPLY=31, SHIFT=5>`.
/// We hardcode multiply=31, shift=5. The INTTYPE is always i32 for
/// last_value; the input/output width varies but the filter itself stores i32.
#[derive(Clone)]
pub struct ScaledFirstOrderFilter {
    last_value: i32,
}

impl ScaledFirstOrderFilter {
    pub fn new() -> Self {
        Self { last_value: 0 }
    }

    pub fn flush(&mut self) {
        self.last_value = 0;
    }

    /// Decompress (inverse filter): used for channel A output.
    /// `last_value = input + (last_value * 31) >> 5; return last_value`
    #[inline(always)]
    pub fn decompress(&mut self, input: i32) -> i32 {
        self.last_value = input.wrapping_add(((self.last_value as i64 * 31) >> 5) as i32);
        self.last_value
    }

    /// Compress (forward filter): used for channel B input DURING decompression.
    /// `result = input - (last_value * 31) >> 5; last_value = input; return result`
    #[inline(always)]
    pub fn compress(&mut self, input: i32) -> i32 {
        let result = input.wrapping_sub(((self.last_value as i64 * 31) >> 5) as i32);
        self.last_value = input;
        result
    }
}

// ===================================================================
// Predictor3950 -- version >= 3950, bitsPerSample < 32
// (INTTYPE=i32, DATATYPE=i16)
// ===================================================================

#[derive(Clone)]
pub struct Predictor3950 {
    // Roll buffers: window=256, history=8
    rb_prediction_a: RollBuffer<i32>,
    rb_prediction_b: RollBuffer<i32>,
    rb_adapt_a: RollBuffer<i32>,
    rb_adapt_b: RollBuffer<i32>,

    // Coefficients
    ary_ma: [i32; M_COUNT],
    ary_mb: [i32; M_COUNT],

    // Filters
    stage1_filter_a: ScaledFirstOrderFilter,
    stage1_filter_b: ScaledFirstOrderFilter,
    nn_filters: Vec<NNFilter16>,

    // State
    last_value_a: i32,
    current_index: i32,
    bits_per_sample: u16,
    interim_mode: bool,
}

impl Predictor3950 {
    /// Create a new predictor for version >= 3950, bitsPerSample < 32.
    ///
    /// * `compression_level` -- APE compression level (1000..5000).
    /// * `version` -- file version number (>= 3950).
    /// * `bits_per_sample` -- 8, 16, or 24.
    pub fn new(compression_level: u32, version: i32, bits_per_sample: u16) -> Self {
        let nn_filters = create_filters_16(compression_level, version);
        let mut p = Self {
            rb_prediction_a: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_prediction_b: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_adapt_a: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_adapt_b: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            ary_ma: INITIAL_MA,
            ary_mb: [0; M_COUNT],
            stage1_filter_a: ScaledFirstOrderFilter::new(),
            stage1_filter_b: ScaledFirstOrderFilter::new(),
            nn_filters,
            last_value_a: 0,
            current_index: 0,
            bits_per_sample,
            interim_mode: false,
        };
        p.flush();
        p
    }

    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
        set_interim_mode_16(&mut self.nn_filters, mode);
    }

    /// Reset all state. Called at the start of each frame.
    pub fn flush(&mut self) {
        self.rb_prediction_a.flush();
        self.rb_prediction_b.flush();
        self.rb_adapt_a.flush();
        self.rb_adapt_b.flush();
        self.ary_ma = INITIAL_MA;
        self.ary_mb = [0; M_COUNT];
        self.stage1_filter_a.flush();
        self.stage1_filter_b.flush();
        flush_cascade_16(&mut self.nn_filters);
        self.last_value_a = 0;
        self.current_index = 0;
    }

    /// Decompress a single sample pair.
    ///
    /// * `n_a` -- entropy-decoded value for channel A (residual).
    /// * `n_b` -- cross-channel value (previous X output, or entropy-decoded B).
    ///
    /// Returns the final PCM sample value.
    pub fn decompress_value(&mut self, n_a: i64, n_b: i64) -> i32 {
        // 1. Roll buffers if window is full
        if self.current_index == WINDOW_BLOCKS as i32 {
            self.rb_prediction_a.roll();
            self.rb_prediction_b.roll();
            self.rb_adapt_a.roll();
            self.rb_adapt_b.roll();
            self.current_index = 0;
        }

        // 2. Cast inputs to working type (i32)
        let mut n_a: i32 = n_a as i32;
        let n_b: i32 = n_b as i32;

        // 3. NNFilter cascade (reverse order: filter2 -> filter1 -> filter0)
        //    Only nA passes through NNFilters; nB does not.
        n_a = decompress_cascade_16(&mut self.nn_filters, n_a);

        // 4. Prediction buffer setup
        //    A buffer: store last_value_a, then overwrite [-1] with delta
        self.rb_prediction_a.set(0, self.last_value_a);
        let pa0 = self.rb_prediction_a.get(0);
        let pa_prev = self.rb_prediction_a.get(-1);
        self.rb_prediction_a.set(-1, pa0.wrapping_sub(pa_prev));

        //    B buffer: compress nB through stage1_filter_b, then delta
        let compressed_b = self.stage1_filter_b.compress(n_b);
        self.rb_prediction_b.set(0, compressed_b);
        let pb0 = self.rb_prediction_b.get(0);
        let pb_prev = self.rb_prediction_b.get(-1);
        self.rb_prediction_b.set(-1, pb0.wrapping_sub(pb_prev));

        // 5. Compute prediction and add to residual
        let n_current_a: i32;

        if self.bits_per_sample <= 16 {
            // Normal path: all arithmetic in i32 (wrapping to match C++ signed overflow)
            let pred_a: i32 = self
                .rb_prediction_a
                .get(0)
                .wrapping_mul(self.ary_ma[0])
                .wrapping_add(self.rb_prediction_a.get(-1).wrapping_mul(self.ary_ma[1]))
                .wrapping_add(self.rb_prediction_a.get(-2).wrapping_mul(self.ary_ma[2]))
                .wrapping_add(self.rb_prediction_a.get(-3).wrapping_mul(self.ary_ma[3]));

            let pred_b: i32 = self
                .rb_prediction_b
                .get(0)
                .wrapping_mul(self.ary_mb[0])
                .wrapping_add(self.rb_prediction_b.get(-1).wrapping_mul(self.ary_mb[1]))
                .wrapping_add(self.rb_prediction_b.get(-2).wrapping_mul(self.ary_mb[2]))
                .wrapping_add(self.rb_prediction_b.get(-3).wrapping_mul(self.ary_mb[3]))
                .wrapping_add(self.rb_prediction_b.get(-4).wrapping_mul(self.ary_mb[4]));

            n_current_a = n_a.wrapping_add((pred_a.wrapping_add(pred_b >> 1)) >> 10);
        } else {
            // High bit-depth path (bitsPerSample > 16, INTTYPE = i32):
            // Widen to i64 for multiplication, then handle truncation.
            let pred_a: i64 = (self.rb_prediction_a.get(0) as i64) * (self.ary_ma[0] as i64)
                + (self.rb_prediction_a.get(-1) as i64) * (self.ary_ma[1] as i64)
                + (self.rb_prediction_a.get(-2) as i64) * (self.ary_ma[2] as i64)
                + (self.rb_prediction_a.get(-3) as i64) * (self.ary_ma[3] as i64);

            let pred_b: i64 = (self.rb_prediction_b.get(0) as i64) * (self.ary_mb[0] as i64)
                + (self.rb_prediction_b.get(-1) as i64) * (self.ary_mb[1] as i64)
                + (self.rb_prediction_b.get(-2) as i64) * (self.ary_mb[2] as i64)
                + (self.rb_prediction_b.get(-3) as i64) * (self.ary_mb[3] as i64)
                + (self.rb_prediction_b.get(-4) as i64) * (self.ary_mb[4] as i64);

            if self.interim_mode {
                // Interim mode: keep full precision
                n_current_a = n_a.wrapping_add(((pred_a + (pred_b >> 1)) >> 10) as i32);
            } else {
                // TRUNCATION: cast pred_a and pred_b to i32 BEFORE combining
                n_current_a =
                    n_a.wrapping_add(((pred_a as i32).wrapping_add((pred_b as i32) >> 1)) >> 10);
            }
        }

        // 6. Compute adapt signs for prediction buffers.
        //    Sign function: ((val >> 30) & 2) - 1 gives +1 for negative, -1
        //    for positive (INVERTED from usual signum).
        let pa0_val = self.rb_prediction_a.get(0);
        self.rb_adapt_a.set(
            0,
            if pa0_val != 0 {
                ((pa0_val >> 30) & 2) - 1
            } else {
                0
            },
        );
        let pa_m1_val = self.rb_prediction_a.get(-1);
        self.rb_adapt_a.set(
            -1,
            if pa_m1_val != 0 {
                ((pa_m1_val >> 30) & 2) - 1
            } else {
                0
            },
        );

        let pb0_val = self.rb_prediction_b.get(0);
        self.rb_adapt_b.set(
            0,
            if pb0_val != 0 {
                ((pb0_val >> 30) & 2) - 1
            } else {
                0
            },
        );
        let pb_m1_val = self.rb_prediction_b.get(-1);
        self.rb_adapt_b.set(
            -1,
            if pb_m1_val != 0 {
                ((pb_m1_val >> 30) & 2) - 1
            } else {
                0
            },
        );

        // 7. Adapt coefficients.
        //    Direction uses post-NNFilter nA (NOT nCurrentA, NOT original _nA).
        let adapt_dir: i32 = (n_a < 0) as i32 - (n_a > 0) as i32;

        self.ary_ma[0] =
            self.ary_ma[0].wrapping_add(self.rb_adapt_a.get(0).wrapping_mul(adapt_dir));
        self.ary_ma[1] =
            self.ary_ma[1].wrapping_add(self.rb_adapt_a.get(-1).wrapping_mul(adapt_dir));
        self.ary_ma[2] =
            self.ary_ma[2].wrapping_add(self.rb_adapt_a.get(-2).wrapping_mul(adapt_dir));
        self.ary_ma[3] =
            self.ary_ma[3].wrapping_add(self.rb_adapt_a.get(-3).wrapping_mul(adapt_dir));

        self.ary_mb[0] =
            self.ary_mb[0].wrapping_add(self.rb_adapt_b.get(0).wrapping_mul(adapt_dir));
        self.ary_mb[1] =
            self.ary_mb[1].wrapping_add(self.rb_adapt_b.get(-1).wrapping_mul(adapt_dir));
        self.ary_mb[2] =
            self.ary_mb[2].wrapping_add(self.rb_adapt_b.get(-2).wrapping_mul(adapt_dir));
        self.ary_mb[3] =
            self.ary_mb[3].wrapping_add(self.rb_adapt_b.get(-3).wrapping_mul(adapt_dir));
        self.ary_mb[4] =
            self.ary_mb[4].wrapping_add(self.rb_adapt_b.get(-4).wrapping_mul(adapt_dir));

        // 8. Stage 1 filter and output.
        let result: i32 = self.stage1_filter_a.decompress(n_current_a);

        // 9. CRITICAL: last_value_a is set to nCurrentA (pre-filter value),
        //    NOT the result of stage1_filter_a.
        self.last_value_a = n_current_a;

        // 10. Advance buffers
        self.rb_prediction_a.increment_fast();
        self.rb_prediction_b.increment_fast();
        self.rb_adapt_a.increment_fast();
        self.rb_adapt_b.increment_fast();
        self.current_index += 1;

        result
    }
}

// ===================================================================
// Predictor3950_32 -- version >= 3950, bitsPerSample >= 32
// (INTTYPE=i64, DATATYPE=i32)
// ===================================================================

#[derive(Clone)]
#[allow(dead_code)]
pub struct Predictor3950_32 {
    // Roll buffers using i64 elements
    rb_prediction_a: RollBuffer<i64>,
    rb_prediction_b: RollBuffer<i64>,
    rb_adapt_a: RollBuffer<i64>,
    rb_adapt_b: RollBuffer<i64>,

    // Coefficients (i64 to match INTTYPE)
    ary_ma: [i64; M_COUNT],
    ary_mb: [i64; M_COUNT],

    // Filters
    stage1_filter_a: ScaledFirstOrderFilter,
    stage1_filter_b: ScaledFirstOrderFilter,
    nn_filters: Vec<NNFilter32>,

    // State
    last_value_a: i64,
    current_index: i32,
    interim_mode: bool,
}

impl Predictor3950_32 {
    pub fn new(compression_level: u32, version: i32) -> Self {
        let nn_filters = create_filters_32(compression_level, version);
        let mut p = Self {
            rb_prediction_a: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_prediction_b: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_adapt_a: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            rb_adapt_b: RollBuffer::new(WINDOW_BLOCKS, PRED_HISTORY),
            ary_ma: [0; M_COUNT],
            ary_mb: [0; M_COUNT],
            stage1_filter_a: ScaledFirstOrderFilter::new(),
            stage1_filter_b: ScaledFirstOrderFilter::new(),
            nn_filters,
            last_value_a: 0,
            current_index: 0,
            interim_mode: false,
        };
        p.flush();
        p
    }

    #[allow(dead_code)]
    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
        set_interim_mode_32(&mut self.nn_filters, mode);
    }

    pub fn flush(&mut self) {
        self.rb_prediction_a.flush();
        self.rb_prediction_b.flush();
        self.rb_adapt_a.flush();
        self.rb_adapt_b.flush();
        self.ary_ma = INITIAL_MA_64;
        self.ary_mb = [0; M_COUNT];
        self.stage1_filter_a.flush();
        self.stage1_filter_b.flush();
        flush_cascade_32(&mut self.nn_filters);
        self.last_value_a = 0;
        self.current_index = 0;
    }

    pub fn decompress_value(&mut self, n_a: i64, n_b: i64) -> i32 {
        // 1. Roll buffers if window is full
        if self.current_index == WINDOW_BLOCKS as i32 {
            self.rb_prediction_a.roll();
            self.rb_prediction_b.roll();
            self.rb_adapt_a.roll();
            self.rb_adapt_b.roll();
            self.current_index = 0;
        }

        // 2. Working values are i64
        let mut n_a: i64 = n_a;
        let n_b: i64 = n_b;

        // 3. NNFilter cascade
        n_a = decompress_cascade_32(&mut self.nn_filters, n_a);

        // 4. Prediction buffer setup
        self.rb_prediction_a.set(0, self.last_value_a);
        let pa0 = self.rb_prediction_a.get(0);
        let pa_prev = self.rb_prediction_a.get(-1);
        self.rb_prediction_a.set(-1, pa0.wrapping_sub(pa_prev));

        let compressed_b = self.stage1_filter_b.compress(n_b as i32);
        self.rb_prediction_b.set(0, compressed_b as i64);
        let pb0 = self.rb_prediction_b.get(0);
        let pb_prev = self.rb_prediction_b.get(-1);
        self.rb_prediction_b.set(-1, pb0.wrapping_sub(pb_prev));

        // 5. Compute prediction (all i64 arithmetic for 32-bit path,
        //    sizeof(INTTYPE) == 8 so we take the "normal" branch)
        let pred_a: i64 = self
            .rb_prediction_a
            .get(0)
            .wrapping_mul(self.ary_ma[0])
            .wrapping_add(self.rb_prediction_a.get(-1).wrapping_mul(self.ary_ma[1]))
            .wrapping_add(self.rb_prediction_a.get(-2).wrapping_mul(self.ary_ma[2]))
            .wrapping_add(self.rb_prediction_a.get(-3).wrapping_mul(self.ary_ma[3]));

        let pred_b: i64 = self
            .rb_prediction_b
            .get(0)
            .wrapping_mul(self.ary_mb[0])
            .wrapping_add(self.rb_prediction_b.get(-1).wrapping_mul(self.ary_mb[1]))
            .wrapping_add(self.rb_prediction_b.get(-2).wrapping_mul(self.ary_mb[2]))
            .wrapping_add(self.rb_prediction_b.get(-3).wrapping_mul(self.ary_mb[3]))
            .wrapping_add(self.rb_prediction_b.get(-4).wrapping_mul(self.ary_mb[4]));

        let n_current_a: i64 = n_a.wrapping_add((pred_a.wrapping_add(pred_b >> 1)) >> 10);

        // 6. Adapt signs (i64 version: shift by 62 for sign bit extraction)
        let pa0_val = self.rb_prediction_a.get(0);
        self.rb_adapt_a.set(
            0,
            if pa0_val != 0 {
                ((pa0_val >> 30) & 2) - 1
            } else {
                0
            },
        );
        let pa_m1_val = self.rb_prediction_a.get(-1);
        self.rb_adapt_a.set(
            -1,
            if pa_m1_val != 0 {
                ((pa_m1_val >> 30) & 2) - 1
            } else {
                0
            },
        );

        let pb0_val = self.rb_prediction_b.get(0);
        self.rb_adapt_b.set(
            0,
            if pb0_val != 0 {
                ((pb0_val >> 30) & 2) - 1
            } else {
                0
            },
        );
        let pb_m1_val = self.rb_prediction_b.get(-1);
        self.rb_adapt_b.set(
            -1,
            if pb_m1_val != 0 {
                ((pb_m1_val >> 30) & 2) - 1
            } else {
                0
            },
        );

        // 7. Adapt coefficients
        let adapt_dir: i64 = (n_a < 0) as i64 - (n_a > 0) as i64;

        self.ary_ma[0] =
            self.ary_ma[0].wrapping_add(self.rb_adapt_a.get(0).wrapping_mul(adapt_dir));
        self.ary_ma[1] =
            self.ary_ma[1].wrapping_add(self.rb_adapt_a.get(-1).wrapping_mul(adapt_dir));
        self.ary_ma[2] =
            self.ary_ma[2].wrapping_add(self.rb_adapt_a.get(-2).wrapping_mul(adapt_dir));
        self.ary_ma[3] =
            self.ary_ma[3].wrapping_add(self.rb_adapt_a.get(-3).wrapping_mul(adapt_dir));

        self.ary_mb[0] =
            self.ary_mb[0].wrapping_add(self.rb_adapt_b.get(0).wrapping_mul(adapt_dir));
        self.ary_mb[1] =
            self.ary_mb[1].wrapping_add(self.rb_adapt_b.get(-1).wrapping_mul(adapt_dir));
        self.ary_mb[2] =
            self.ary_mb[2].wrapping_add(self.rb_adapt_b.get(-2).wrapping_mul(adapt_dir));
        self.ary_mb[3] =
            self.ary_mb[3].wrapping_add(self.rb_adapt_b.get(-3).wrapping_mul(adapt_dir));
        self.ary_mb[4] =
            self.ary_mb[4].wrapping_add(self.rb_adapt_b.get(-4).wrapping_mul(adapt_dir));

        // 8. Stage 1 filter and output
        let result: i32 = self.stage1_filter_a.decompress(n_current_a as i32);

        // 9. CRITICAL: last_value_a is pre-filter value
        self.last_value_a = n_current_a;

        // 10. Advance buffers
        self.rb_prediction_a.increment_fast();
        self.rb_prediction_b.increment_fast();
        self.rb_adapt_a.increment_fast();
        self.rb_adapt_b.increment_fast();
        self.current_index += 1;

        result
    }
}
