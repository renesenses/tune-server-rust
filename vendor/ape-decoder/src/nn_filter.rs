/// Neural Network Filter for the Monkey's Audio decoder.
///
/// Two concrete types instead of generics, matching the C++ template
/// instantiations:
///
/// - `NNFilter16` -- for 8/16/24-bit audio (INTTYPE=i32, DATATYPE=i16)
/// - `NNFilter32` -- for 32-bit audio (INTTYPE=i64, DATATYPE=i32)
///
/// Reference: `NNFilter.h`, `NNFilter.cpp`, `NNFilterGeneric.cpp`,
/// `NNFilterCommon.h`.
use crate::roll_buffer::RollBuffer;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Clamp an i32 value to i16 range using the C++ bit-trick.
#[inline(always)]
fn get_saturated_short_from_i32(value: i32) -> i16 {
    let s = value as i16;
    if s as i32 != value {
        ((value >> 31) ^ 0x7FFF) as i16
    } else {
        s
    }
}

/// Clamp an i64 value to i16 range using the C++ bit-trick.
#[inline(always)]
fn get_saturated_short_from_i64(value: i64) -> i16 {
    let s = value as i16;
    if s as i64 != value {
        ((value >> 63) ^ 0x7FFF) as i16
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Dot-product functions
// ---------------------------------------------------------------------------

/// Dot product for <i32, i16> path: i16*i16 accumulated in i32.
#[inline(always)]
fn calculate_dot_product_16(a: &[i16], b: &[i16], order: usize) -> i32 {
    let mut dot: i32 = 0;
    for i in 0..order {
        dot += (a[i] as i32) * (b[i] as i32);
    }
    dot
}

/// Dot product for <i64, i32> path: each i32*i32 TRUNCATES to i32 via
/// wrapping_mul BEFORE widening to i64 for accumulation.
#[inline(always)]
fn calculate_dot_product_32(a: &[i32], b: &[i32], order: usize) -> i64 {
    let mut dot: i64 = 0;
    for i in 0..order {
        let temp: i32 = a[i].wrapping_mul(b[i]);
        dot += temp as i64;
    }
    dot
}

// ---------------------------------------------------------------------------
// Adapt functions
// ---------------------------------------------------------------------------

/// Adapt weights for <i32, i16> path.
#[inline(always)]
fn adapt_16(m: &mut [i16], delta: &[i16], direction: i32, order: usize) {
    if direction < 0 {
        for i in 0..order {
            m[i] = m[i].wrapping_add(delta[i]);
        }
    } else if direction > 0 {
        for i in 0..order {
            m[i] = m[i].wrapping_sub(delta[i]);
        }
    }
}

/// Adapt weights for <i64, i32> path.
#[inline(always)]
fn adapt_32(m: &mut [i32], delta: &[i32], direction: i64, order: usize) {
    if direction < 0 {
        for i in 0..order {
            m[i] = m[i].wrapping_add(delta[i]);
        }
    } else if direction > 0 {
        for i in 0..order {
            m[i] = m[i].wrapping_sub(delta[i]);
        }
    }
}

// ===================================================================
// NNFilter16 -- for 8/16/24-bit audio (INTTYPE=i32, DATATYPE=i16)
// ===================================================================

const NN_WINDOW: usize = 512;

#[derive(Clone)]
pub struct NNFilter16 {
    order: usize,
    shift: i32,
    one_shifted: i32,
    version: i32,
    weights: Vec<i16>,
    rb_input: RollBuffer<i16>,
    rb_delta_m: RollBuffer<i16>,
    running_average: i32,
    interim_mode: bool,
}

impl NNFilter16 {
    /// Create a new 16-bit NN filter.
    ///
    /// * `order` -- must be 16 or a multiple of 32.
    /// * `shift` -- right-shift applied after dot product.
    /// * `version` -- file version; -1 means "current" (>= 3980 behaviour).
    pub fn new(order: usize, shift: i32, version: i32) -> Self {
        assert!(
            order > 0 && (order == 16 || order % 32 == 0),
            "NNFilter16: order must be 16 or a multiple of 32, got {}",
            order
        );
        Self {
            order,
            shift,
            one_shifted: 1i32 << (shift - 1),
            version,
            weights: vec![0i16; order],
            rb_input: RollBuffer::new(NN_WINDOW, order),
            rb_delta_m: RollBuffer::new(NN_WINDOW, order),
            running_average: 0,
            interim_mode: false,
        }
    }

    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
    }

    /// Reset all state. Called at the start of each frame.
    pub fn flush(&mut self) {
        self.weights.iter_mut().for_each(|w| *w = 0);
        self.rb_input.flush();
        self.rb_delta_m.flush();
        self.running_average = 0;
    }

    /// Core decompression: takes an encoded residual, returns the reconstructed
    /// sample value.
    pub fn decompress(&mut self, input: i32) -> i32 {
        // 1. Dot product over history
        let input_hist = self.rb_input.slice(-(self.order as isize), self.order);
        let dot_product = calculate_dot_product_16(input_hist, &self.weights, self.order);

        // 2. Compute output (prediction + residual)
        let output: i32 = if self.interim_mode {
            // Widen to i64 before adding rounding bias and shifting
            input + ((dot_product as i64 + self.one_shifted as i64) >> self.shift) as i32
        } else {
            input + ((dot_product + self.one_shifted) >> self.shift)
        };

        // 3. Adapt weights -- CRITICAL: uses INPUT (the residual), NOT output
        {
            let delta_slice = self.rb_delta_m.slice(-(self.order as isize), self.order);
            adapt_16(&mut self.weights, delta_slice, input, self.order);
        }

        // 4. Update delta buffer -- CRITICAL: uses OUTPUT (reconstructed sample)
        if self.version == -1 || self.version >= 3980 {
            self.update_delta_new(output);
        } else {
            self.update_delta_old(output);
        }

        // 5. Store saturated value in input history
        self.rb_input.set(0, get_saturated_short_from_i32(output));

        // 6. Advance both roll buffers
        self.rb_input.increment_safe();
        self.rb_delta_m.increment_safe();

        output
    }

    /// UPDATE_DELTA_NEW (version >= 3980 or version == -1).
    fn update_delta_new(&mut self, value: i32) {
        let abs_value = value.abs();

        if abs_value > self.running_average * 3 {
            self.rb_delta_m.set(0, (((value >> 25) & 64) - 32) as i16);
        } else if abs_value > (self.running_average * 4) / 3 {
            self.rb_delta_m.set(0, (((value >> 26) & 32) - 16) as i16);
        } else if abs_value > 0 {
            self.rb_delta_m.set(0, (((value >> 27) & 16) - 8) as i16);
        } else {
            self.rb_delta_m.set(0, 0);
        }

        // Exponential moving average (integer division truncates toward zero)
        self.running_average += (abs_value - self.running_average) / 16;

        // Decay historical deltas at positions [-1], [-2], [-8]
        *self.rb_delta_m.get_mut(-1) >>= 1;
        *self.rb_delta_m.get_mut(-2) >>= 1;
        *self.rb_delta_m.get_mut(-8) >>= 1;
    }

    /// UPDATE_DELTA_OLD (version < 3980).
    fn update_delta_old(&mut self, value: i32) {
        if value == 0 {
            self.rb_delta_m.set(0, 0);
        } else {
            self.rb_delta_m.set(0, (((value >> 28) & 8) - 4) as i16);
        }

        // Decay historical deltas at positions [-4], [-8]
        *self.rb_delta_m.get_mut(-4) >>= 1;
        *self.rb_delta_m.get_mut(-8) >>= 1;
    }
}

// ===================================================================
// NNFilter32 -- for 32-bit audio (INTTYPE=i64, DATATYPE=i32)
// ===================================================================

#[derive(Clone)]
pub struct NNFilter32 {
    order: usize,
    shift: i32,
    one_shifted: i32,
    version: i32,
    weights: Vec<i32>,
    rb_input: RollBuffer<i32>,
    rb_delta_m: RollBuffer<i32>,
    running_average: i64,
    interim_mode: bool,
}

impl NNFilter32 {
    /// Create a new 32-bit NN filter.
    pub fn new(order: usize, shift: i32, version: i32) -> Self {
        assert!(
            order > 0 && (order == 16 || order % 32 == 0),
            "NNFilter32: order must be 16 or a multiple of 32, got {}",
            order
        );
        Self {
            order,
            shift,
            one_shifted: 1i32 << (shift - 1),
            version,
            weights: vec![0i32; order],
            rb_input: RollBuffer::new(NN_WINDOW, order),
            rb_delta_m: RollBuffer::new(NN_WINDOW, order),
            running_average: 0,
            interim_mode: false,
        }
    }

    #[allow(dead_code)]
    pub fn set_interim_mode(&mut self, mode: bool) {
        self.interim_mode = mode;
    }

    pub fn flush(&mut self) {
        self.weights.iter_mut().for_each(|w| *w = 0);
        self.rb_input.flush();
        self.rb_delta_m.flush();
        self.running_average = 0;
    }

    pub fn decompress(&mut self, input: i64) -> i64 {
        // 1. Dot product
        let input_hist = self.rb_input.slice(-(self.order as isize), self.order);
        let dot_product = calculate_dot_product_32(input_hist, &self.weights, self.order);

        // 2. Compute output
        let output: i64 = if self.interim_mode {
            input + ((dot_product as i64 + self.one_shifted as i64) >> self.shift)
        } else {
            input + ((dot_product + self.one_shifted as i64) >> self.shift)
        };

        // 3. Adapt weights -- CRITICAL: uses INPUT (the residual)
        {
            let delta_slice = self.rb_delta_m.slice(-(self.order as isize), self.order);
            adapt_32(&mut self.weights, delta_slice, input, self.order);
        }

        // 4. Update delta buffer -- uses OUTPUT
        if self.version == -1 || self.version >= 3980 {
            self.update_delta_new(output);
        } else {
            self.update_delta_old(output);
        }

        // 5. Store saturated value in input history.
        // For <i64, i32>: DATATYPE is i32, but GetSaturatedShortFromInt still
        // returns i16 which is then widened to i32 on assignment.
        self.rb_input
            .set(0, get_saturated_short_from_i64(output) as i32);

        // 6. Advance
        self.rb_input.increment_safe();
        self.rb_delta_m.increment_safe();

        output
    }

    fn update_delta_new(&mut self, value: i64) {
        let abs_value = value.abs();

        // The shifts (25/26/27) operate on the full i64 width. Sign extraction
        // only works correctly when the value fits in 32 bits (which it
        // generally does after saturation clamping).
        if abs_value > self.running_average * 3 {
            self.rb_delta_m.set(0, (((value >> 25) & 64) - 32) as i32);
        } else if abs_value > (self.running_average * 4) / 3 {
            self.rb_delta_m.set(0, (((value >> 26) & 32) - 16) as i32);
        } else if abs_value > 0 {
            self.rb_delta_m.set(0, (((value >> 27) & 16) - 8) as i32);
        } else {
            self.rb_delta_m.set(0, 0);
        }

        self.running_average += (abs_value - self.running_average) / 16;

        *self.rb_delta_m.get_mut(-1) >>= 1;
        *self.rb_delta_m.get_mut(-2) >>= 1;
        *self.rb_delta_m.get_mut(-8) >>= 1;
    }

    fn update_delta_old(&mut self, value: i64) {
        if value == 0 {
            self.rb_delta_m.set(0, 0);
        } else {
            self.rb_delta_m.set(0, (((value >> 28) & 8) - 4) as i32);
        }

        *self.rb_delta_m.get_mut(-4) >>= 1;
        *self.rb_delta_m.get_mut(-8) >>= 1;
    }
}

// ===================================================================
// Filter cascade helpers
// ===================================================================

/// Compression level constants.
pub const COMPRESSION_FAST: u32 = 1000;
pub const COMPRESSION_NORMAL: u32 = 2000;
pub const COMPRESSION_HIGH: u32 = 3000;
pub const COMPRESSION_EXTRA_HIGH: u32 = 4000;
pub const COMPRESSION_INSANE: u32 = 5000;

/// Filter configuration: (order, shift).
type FilterConfig = (usize, i32);

/// Return the filter configurations for a given compression level.
/// The returned list is in creation order (largest filter first for
/// multi-filter levels).
pub fn filter_configs(compression_level: u32) -> Vec<FilterConfig> {
    match compression_level {
        COMPRESSION_FAST => vec![],
        COMPRESSION_NORMAL => vec![(16, 11)],
        COMPRESSION_HIGH => vec![(64, 11)],
        COMPRESSION_EXTRA_HIGH => vec![(256, 13), (32, 10)],
        COMPRESSION_INSANE => vec![(1280, 15), (256, 13), (16, 11)],
        _ => vec![],
    }
}

/// Create NNFilter16 instances for a compression level.
/// Returns filters in creation order; call `decompress_cascade_16` to apply
/// them in the correct (reverse) order.
pub fn create_filters_16(compression_level: u32, version: i32) -> Vec<NNFilter16> {
    filter_configs(compression_level)
        .into_iter()
        .map(|(order, shift)| NNFilter16::new(order, shift, version))
        .collect()
}

/// Create NNFilter32 instances for a compression level.
pub fn create_filters_32(compression_level: u32, version: i32) -> Vec<NNFilter32> {
    filter_configs(compression_level)
        .into_iter()
        .map(|(order, shift)| NNFilter32::new(order, shift, version))
        .collect()
}

/// Apply the NNFilter16 cascade in decompression order (reverse of creation:
/// last/smallest filter first, then toward the largest).
#[inline]
pub fn decompress_cascade_16(filters: &mut [NNFilter16], mut value: i32) -> i32 {
    for f in filters.iter_mut().rev() {
        value = f.decompress(value);
    }
    value
}

/// Apply the NNFilter32 cascade in decompression order.
#[inline]
pub fn decompress_cascade_32(filters: &mut [NNFilter32], mut value: i64) -> i64 {
    for f in filters.iter_mut().rev() {
        value = f.decompress(value);
    }
    value
}

/// Flush all filters in a cascade.
pub fn flush_cascade_16(filters: &mut [NNFilter16]) {
    for f in filters.iter_mut() {
        f.flush();
    }
}

pub fn flush_cascade_32(filters: &mut [NNFilter32]) {
    for f in filters.iter_mut() {
        f.flush();
    }
}

/// Set interim mode on all filters in a cascade.
pub fn set_interim_mode_16(filters: &mut [NNFilter16], mode: bool) {
    for f in filters.iter_mut() {
        f.set_interim_mode(mode);
    }
}

#[allow(dead_code)]
pub fn set_interim_mode_32(filters: &mut [NNFilter32], mode: bool) {
    for f in filters.iter_mut() {
        f.set_interim_mode(mode);
    }
}
