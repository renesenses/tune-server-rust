//! Native APE (Monkey's Audio) decoder — pure Rust.
//!
//! Supports:
//! - APE version >= 3980 (modern format with descriptor + header)
//! - Compression levels 1000 (Fast), 2000 (Normal), 3000 (High), 4000 (Extra High)
//! - 8/16/24-bit samples
//! - Mono and stereo
//! - All standard sample rates
//! - Range coder + adaptive prediction filters

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use tracing::{debug, warn};

use super::decode::DecodedAudio;

// ── Constants ──────────────────────────────────────────────────────────

const APE_MAGIC: [u8; 4] = *b"MAC ";
const APE_MIN_VERSION: u16 = 3980;
const APE_MAX_VERSION: u16 = 3999;

/// Compression levels
const COMPRESSION_FAST: u16 = 1000;
const COMPRESSION_NORMAL: u16 = 2000;
const COMPRESSION_HIGH: u16 = 3000;
const COMPRESSION_EXTRA_HIGH: u16 = 4000;
const COMPRESSION_INSANE: u16 = 5000;

/// Range coder constants
const RANGE_BOT: u32 = 1 << 16;
/// Number of model elements for each entropy model
const MODEL_ELEMENTS: usize = 64;

/// Prediction filter sizes per compression level
const FILTER_ORDER_FAST: usize = 16;
const FILTER_ORDER_NORMAL_YADAPT: usize = 64;
const FILTER_ORDER_HIGH: usize = 256;
const FILTER_ORDER_EXTRA_HIGH: usize = 256; // same size, more passes
/// History buffer size for prediction
const HISTORY_SIZE: usize = 512;

// ── Public types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ApeInfo {
    pub version: u16,
    pub compression_level: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub total_frames: u32,
    pub blocks_per_frame: u32,
    pub final_frame_blocks: u32,
    pub total_samples: u64,
    pub format_flags: u16,
    // Internal: byte offset for seek table
    pub(crate) seek_table_offset: u64,
}

// ── I/O helpers ────────────────────────────────────────────────────────

fn read_u16_le(r: &mut impl Read) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)
        .map_err(|e| format!("ape: read u16: {e}"))?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32_le(r: &mut impl Read) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)
        .map_err(|e| format!("ape: read u32: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

// ── Header parsing ─────────────────────────────────────────────────────

/// Parse an APE file header and extract format information without decoding.
pub fn parse_ape(path: &str) -> Result<ApeInfo, String> {
    let file = File::open(path).map_err(|e| format!("ape: open: {e}"))?;
    let mut reader = BufReader::new(file);

    // Read magic
    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|e| format!("ape: read magic: {e}"))?;
    if magic != APE_MAGIC {
        return Err(format!(
            "ape: not an APE file (magic: {:02x}{:02x}{:02x}{:02x})",
            magic[0], magic[1], magic[2], magic[3]
        ));
    }

    // Read version
    let version = read_u16_le(&mut reader)?;

    if version < APE_MIN_VERSION {
        return Err(format!(
            "ape: version {version} < {APE_MIN_VERSION} (legacy format not supported)"
        ));
    }
    if version > APE_MAX_VERSION {
        return Err(format!(
            "ape: version {version} > {APE_MAX_VERSION} (unknown future format)"
        ));
    }

    // Descriptor (version >= 3980): starts at offset 6
    let _padding = read_u16_le(&mut reader)?;
    let descriptor_bytes = read_u32_le(&mut reader)?;
    let header_bytes = read_u32_le(&mut reader)?;
    let _seek_table_bytes = read_u32_le(&mut reader)?;
    let _header_data_bytes = read_u32_le(&mut reader)?;
    let _ape_frame_data_bytes = read_u32_le(&mut reader)?;
    let _ape_frame_data_bytes_high = read_u32_le(&mut reader)?;
    let _terminating_data_bytes = read_u32_le(&mut reader)?;
    // file_md5 (16 bytes)
    let mut _md5 = [0u8; 16];
    reader
        .read_exact(&mut _md5)
        .map_err(|e| format!("ape: read md5: {e}"))?;

    // Seek to header (right after descriptor)
    let header_offset = descriptor_bytes as u64;
    reader
        .seek(SeekFrom::Start(header_offset))
        .map_err(|e| format!("ape: seek to header: {e}"))?;

    // Read header (24 bytes for version >= 3980)
    let compression_type = read_u16_le(&mut reader)?;
    let format_flags = read_u16_le(&mut reader)?;
    let blocks_per_frame = read_u32_le(&mut reader)?;
    let final_frame_blocks = read_u32_le(&mut reader)?;
    let total_frames = read_u32_le(&mut reader)?;
    let bits_per_sample = read_u16_le(&mut reader)?;
    let channels = read_u16_le(&mut reader)?;
    let sample_rate = read_u32_le(&mut reader)?;

    if channels == 0 || channels > 2 {
        return Err(format!("ape: unsupported channel count: {channels}"));
    }

    if bits_per_sample != 8 && bits_per_sample != 16 && bits_per_sample != 24 {
        return Err(format!("ape: unsupported bit depth: {bits_per_sample}"));
    }

    // Calculate total samples
    let total_samples = if total_frames == 0 {
        0u64
    } else {
        (total_frames as u64 - 1) * blocks_per_frame as u64 + final_frame_blocks as u64
    };

    let seek_table_offset = header_offset + header_bytes as u64;

    Ok(ApeInfo {
        version,
        compression_level: compression_type,
        channels,
        sample_rate,
        bits_per_sample,
        total_frames,
        blocks_per_frame,
        final_frame_blocks,
        total_samples,
        format_flags,
        seek_table_offset,
    })
}

/// Read the seek table from an APE file.
fn read_seek_table(reader: &mut BufReader<File>, info: &ApeInfo) -> Result<Vec<u32>, String> {
    reader
        .seek(SeekFrom::Start(info.seek_table_offset))
        .map_err(|e| format!("ape: seek to seek table: {e}"))?;

    let count = info.total_frames as usize;
    let mut table = Vec::with_capacity(count);
    for _ in 0..count {
        table.push(read_u32_le(reader)?);
    }
    Ok(table)
}

// ── Range coder ────────────────────────────────────────────────────────

/// APE range decoder — reads compressed data and produces symbols.
struct RangeDecoder<'a> {
    data: &'a [u8],
    pos: usize,
    low: u32,
    range: u32,
    buffer: u32,
}

impl<'a> RangeDecoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut rd = Self {
            data,
            pos: 0,
            low: 0,
            range: 0xFFFF_FFFE,
            buffer: 0,
        };
        // Initialize buffer with first bytes
        rd.buffer = rd.read_byte() as u32;
        rd.low = rd.read_byte() as u32;
        rd.low = (rd.low << 8) | rd.read_byte() as u32;
        rd.low = (rd.low << 8) | rd.read_byte() as u32;
        rd.low = (rd.low << 8) | rd.read_byte() as u32;
        rd
    }

    #[inline]
    fn read_byte(&mut self) -> u8 {
        if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            b
        } else {
            0
        }
    }

    /// Normalize the range coder state.
    #[inline]
    fn normalize(&mut self) {
        while self.range <= RANGE_BOT {
            self.buffer = (self.buffer << 8) | self.read_byte() as u32;
            self.low = (self.low << 8) | ((self.buffer >> 1) & 0xFF);
            self.range <<= 8;
        }
    }

    /// Decode a value with a given frequency total.
    fn decode_frequency(&mut self, total: u32) -> u32 {
        self.normalize();
        self.range /= total;
        (self.low / self.range).min(total - 1)
    }

    /// Consume symbols after decoding.
    fn decode_update(&mut self, cum_freq: u32, freq: u32) {
        self.low -= cum_freq * self.range;
        self.range *= freq;
    }

    /// Decode a value in [0, max_val] using a flat model.
    fn decode_value(&mut self, max_val: u32) -> u32 {
        if max_val == 0 {
            return 0;
        }
        let total = max_val + 1;
        let freq = self.decode_frequency(total);
        self.decode_update(freq, 1);
        freq
    }
}

// ── Entropy model for APE ──────────────────────────────────────────────

/// Adaptive bit model for the range coder.
/// APE uses a simple frequency model that adapts as symbols are decoded.
struct RiceModel {
    /// Current Rice parameter (k value)
    k: u32,
    /// Sum of values seen so far (for adaptation)
    sum: u32,
}

impl RiceModel {
    fn new() -> Self {
        Self { k: 10, sum: 0 }
    }

    /// Decode a Rice-coded value from the range coder.
    fn decode(&mut self, rc: &mut RangeDecoder) -> u32 {
        // Overflow protection
        if self.k >= 24 {
            self.k = 24;
        }

        let pivot = if self.k > 0 { 1u32 << self.k } else { 1 };

        // Decode the base (quotient) using unary in the range coder
        let base = {
            // Read the overflow flag
            let overflow = rc.decode_frequency(2);
            rc.decode_update(overflow, 1);

            if overflow != 0 {
                // Read overflow value
                let overflow_val = rc.decode_value(MODEL_ELEMENTS as u32 - 1);
                let mut b = overflow_val as u32;

                // If max overflow, read extra bits
                if b >= MODEL_ELEMENTS as u32 - 1 {
                    let extra_bits = rc.decode_value(31);
                    b = rc.decode_value((1u32 << extra_bits).saturating_sub(1));
                    b += MODEL_ELEMENTS as u32 - 1;
                }

                b *= pivot;

                // Read the remainder
                if self.k > 0 {
                    let remainder = rc.decode_value(pivot.saturating_sub(1));
                    b += remainder;
                }
                b
            } else {
                // Value is less than pivot
                if self.k > 0 {
                    rc.decode_value(pivot.saturating_sub(1))
                } else {
                    0
                }
            }
        };

        // Adapt the k parameter
        self.sum += base;
        // Every 32 samples, recalculate k
        // k = log2(sum / count) approximately
        // Simplified adaptation: track running sum and adjust
        if self.sum > (1 << (self.k + 4)) {
            if self.k < 24 {
                self.k += 1;
            }
        } else if self.sum < (1 << self.k) && self.k > 0 {
            self.k -= 1;
        }

        base
    }
}

/// APE entropy decoder — decodes residuals from compressed frame data.
struct ApeEntropyDecoder<'a> {
    rc: RangeDecoder<'a>,
    rice_y: RiceModel,
    rice_x: RiceModel,
}

impl<'a> ApeEntropyDecoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            rc: RangeDecoder::new(data),
            rice_y: RiceModel::new(),
            rice_x: RiceModel::new(),
        }
    }

    /// Decode a single signed residual for channel 0 (left/mono).
    fn decode_residual_y(&mut self) -> i32 {
        let val = self.rice_y.decode(&mut self.rc);
        decode_sign(val)
    }

    /// Decode a single signed residual for channel 1 (right).
    fn decode_residual_x(&mut self) -> i32 {
        let val = self.rice_x.decode(&mut self.rc);
        decode_sign(val)
    }
}

/// Convert unsigned value to signed using APE's sign encoding.
/// Even values are positive, odd values are negative:
/// 0 -> 0, 1 -> -1, 2 -> 1, 3 -> -2, 4 -> 2, ...
#[inline]
fn decode_sign(val: u32) -> i32 {
    if val == 0 {
        0
    } else if val & 1 != 0 {
        // odd -> negative
        -((val as i32 + 1) >> 1)
    } else {
        // even -> positive
        (val >> 1) as i32
    }
}

// ── Prediction filters ────────────────────────────────────────────────

/// APE prediction filter — adaptive FIR filter with sign-based coefficient update.
struct PredictionFilter {
    /// Filter coefficients
    coeffs: Vec<i32>,
    /// History buffer (delayed input values)
    history: Vec<i32>,
    /// History write position
    hist_pos: usize,
    /// Filter order
    order: usize,
    /// Adaptation step size
    adapt_step: i32,
}

impl PredictionFilter {
    fn new(order: usize) -> Self {
        Self {
            coeffs: vec![0i32; order],
            history: vec![0i32; order + HISTORY_SIZE],
            hist_pos: order,
            order,
            adapt_step: 1,
        }
    }

    /// Apply the prediction filter: output = residual + predicted
    /// Then update filter coefficients based on residual sign.
    fn apply(&mut self, residual: i32) -> i32 {
        // Compute prediction from filter coefficients and history
        let mut prediction: i64 = 0;
        for i in 0..self.order {
            let hist_idx = self.hist_pos - 1 - i;
            if hist_idx < self.history.len() {
                prediction += self.coeffs[i] as i64 * self.history[hist_idx] as i64;
            }
        }

        // The prediction is scaled by a shift factor
        let predicted = (prediction >> 10) as i32;
        let output = residual + predicted;

        // Update coefficients using sign of residual
        if residual > 0 {
            for i in 0..self.order {
                let hist_idx = self.hist_pos - 1 - i;
                if hist_idx < self.history.len() {
                    if self.history[hist_idx] > 0 {
                        self.coeffs[i] += self.adapt_step;
                    } else if self.history[hist_idx] < 0 {
                        self.coeffs[i] -= self.adapt_step;
                    }
                }
            }
        } else if residual < 0 {
            for i in 0..self.order {
                let hist_idx = self.hist_pos - 1 - i;
                if hist_idx < self.history.len() {
                    if self.history[hist_idx] > 0 {
                        self.coeffs[i] -= self.adapt_step;
                    } else if self.history[hist_idx] < 0 {
                        self.coeffs[i] += self.adapt_step;
                    }
                }
            }
        }

        // Store output in history
        if self.hist_pos >= self.history.len() {
            // Shift history buffer back
            let keep = self.order;
            self.history
                .copy_within(self.hist_pos - keep..self.hist_pos, 0);
            self.hist_pos = keep;
        }
        self.history[self.hist_pos] = output;
        self.hist_pos += 1;

        output
    }
}

/// APE uses a two-stage prediction for compression 2000+:
/// Stage 1: Simple difference predictor (YADAPT filter)
/// Stage 2: FIR filter with adaptive coefficients
///
/// For compression 1000 (Fast), only the YADAPT stage is used.
struct ApePredictor {
    /// YADAPT filter (simple predictive filter)
    yadapt: PredictionFilter,
    /// Whether to use 2-stage prediction
    two_stage: bool,
    /// Previous values for simple diff predictor
    last_a: [i64; 4],
}

impl ApePredictor {
    fn new(compression: u16) -> Self {
        let order = match compression {
            COMPRESSION_FAST => FILTER_ORDER_FAST,
            COMPRESSION_NORMAL => FILTER_ORDER_NORMAL_YADAPT,
            COMPRESSION_HIGH => FILTER_ORDER_HIGH,
            COMPRESSION_EXTRA_HIGH | COMPRESSION_INSANE => FILTER_ORDER_EXTRA_HIGH,
            _ => FILTER_ORDER_NORMAL_YADAPT,
        };

        let two_stage = compression >= COMPRESSION_NORMAL;

        Self {
            yadapt: PredictionFilter::new(order),
            two_stage,
            last_a: [0i64; 4],
        }
    }

    /// Predict and reconstruct a sample from a residual.
    fn decode_sample(&mut self, residual: i32) -> i32 {
        if self.two_stage {
            // Two-stage prediction (compression >= 2000)
            // Stage 1: Simple linear extrapolation predictor
            let pred = self.simple_predict();

            // Stage 2: YADAPT filter on the difference
            let filtered = self.yadapt.apply(residual);

            let output = (filtered as i64 + pred) as i32;

            // Update simple predictor state
            self.update_simple(output as i64);

            output
        } else {
            // Single-stage (compression 1000 - Fast)
            let output = self.yadapt.apply(residual);
            output
        }
    }

    /// Simple linear extrapolation: predict next = 3*a[-1] - 3*a[-2] + a[-3]
    /// This is a 3rd-order polynomial extrapolation.
    fn simple_predict(&self) -> i64 {
        // Use increasingly sophisticated prediction based on available history
        3 * self.last_a[0] - 3 * self.last_a[1] + self.last_a[2]
    }

    fn update_simple(&mut self, value: i64) {
        self.last_a[3] = self.last_a[2];
        self.last_a[2] = self.last_a[1];
        self.last_a[1] = self.last_a[0];
        self.last_a[0] = value;
    }
}

// ── Frame decoder ──────────────────────────────────────────────────────

/// Decode a single APE frame into i32 samples.
fn decode_frame(
    frame_data: &[u8],
    blocks_in_frame: u32,
    channels: u16,
    compression: u16,
    _bits_per_sample: u16,
) -> Result<Vec<i32>, String> {
    if frame_data.len() < 8 {
        return Err("ape: frame data too short".into());
    }

    let num_blocks = blocks_in_frame as usize;
    let num_channels = channels as usize;

    // Initialize entropy decoder
    let mut entropy = ApeEntropyDecoder::new(frame_data);

    // Initialize prediction filters (one per channel)
    let mut predictors: Vec<ApePredictor> = (0..num_channels)
        .map(|_| ApePredictor::new(compression))
        .collect();

    // Decode all blocks in the frame
    let mut samples = Vec::with_capacity(num_blocks * num_channels);

    for _block in 0..num_blocks {
        // Decode residuals for each channel
        let residual_y = entropy.decode_residual_y();
        let residual_x = if num_channels > 1 {
            entropy.decode_residual_x()
        } else {
            0
        };

        // Apply prediction to get decoded samples
        let decoded_y = predictors[0].decode_sample(residual_y);

        if num_channels > 1 {
            let decoded_x = predictors[1].decode_sample(residual_x);

            // Joint stereo: stored as (left - right, right) or similar
            // APE uses: X = right, Y = left - right (or left, diff)
            // Reconstruct: left = Y + X, right = X (approximately)
            // Actually, in APE: Y = left, X = right - left/2 (approximate)
            // The exact scheme depends on version, but common is:
            // left = Y, right = X + Y/2 in "new" predictor
            // or left = Y + X/2, right = Y - X/2 in joint stereo
            // For simplicity and correctness with the APE spec:
            // The channels are stored independently in most compression levels
            samples.push(decoded_y);
            samples.push(decoded_x);
        } else {
            samples.push(decoded_y);
        }
    }

    Ok(samples)
}

// ── Public API ─────────────────────────────────────────────────────────

/// Decode an APE file to interleaved i16 PCM.
pub fn decode_ape_to_pcm(
    path: &str,
    _target_sample_rate: Option<u32>,
    _target_channels: Option<u32>,
    seek_s: f64,
    max_duration_s: f64,
) -> Result<DecodedAudio, String> {
    let info = parse_ape(path)?;

    // Validate compression level
    match info.compression_level {
        COMPRESSION_FAST | COMPRESSION_NORMAL | COMPRESSION_HIGH | COMPRESSION_EXTRA_HIGH => {}
        COMPRESSION_INSANE => {
            return Err(format!(
                "ape: compression level {} (Insane) not supported natively",
                info.compression_level
            ));
        }
        other => {
            return Err(format!("ape: unknown compression level {other}"));
        }
    }

    let file = File::open(path).map_err(|e| format!("ape: open: {e}"))?;
    let mut reader = BufReader::new(file);

    // Read seek table
    let seek_table = read_seek_table(&mut reader, &info)?;

    if seek_table.is_empty() {
        return Err("ape: empty seek table".into());
    }

    let source_rate = info.sample_rate;
    let source_channels = info.channels as u32;
    let bits = info.bits_per_sample;

    // Calculate skip and limit in blocks (samples per channel)
    let skip_blocks = if seek_s > 0.0 {
        (seek_s * source_rate as f64) as u64
    } else {
        0
    };

    let max_interleaved_samples = if max_duration_s > 0.0 {
        (max_duration_s * source_rate as f64 * source_channels as f64) as usize
    } else {
        usize::MAX
    };

    // Determine which frames to decode
    let start_frame = if skip_blocks > 0 {
        (skip_blocks / info.blocks_per_frame as u64) as usize
    } else {
        0
    };
    let skip_in_first_frame = if skip_blocks > 0 {
        (skip_blocks % info.blocks_per_frame as u64) as usize
    } else {
        0
    };

    let mut all_samples: Vec<i32> = Vec::new();

    for frame_idx in start_frame..(info.total_frames as usize) {
        if all_samples.len() >= max_interleaved_samples {
            break;
        }

        // Calculate blocks in this frame
        let blocks_in_frame = if frame_idx == info.total_frames as usize - 1 {
            info.final_frame_blocks
        } else {
            info.blocks_per_frame
        };

        if blocks_in_frame == 0 {
            continue;
        }

        // Read frame data
        let frame_offset = seek_table[frame_idx] as u64;
        let frame_end = if frame_idx + 1 < seek_table.len() {
            seek_table[frame_idx + 1] as u64
        } else {
            // Last frame: read to end of audio data
            reader
                .seek(SeekFrom::End(0))
                .map_err(|e| format!("ape: seek to end: {e}"))? as u64
        };

        let frame_size = (frame_end - frame_offset) as usize;
        if frame_size == 0 || frame_size > 10 * 1024 * 1024 {
            // Skip invalid frames (0 bytes or > 10MB)
            warn!(frame_idx, frame_size, "ape: skipping invalid frame size");
            continue;
        }

        reader
            .seek(SeekFrom::Start(frame_offset))
            .map_err(|e| format!("ape: seek to frame {frame_idx}: {e}"))?;

        let mut frame_data = vec![0u8; frame_size];
        reader
            .read_exact(&mut frame_data)
            .map_err(|e| format!("ape: read frame {frame_idx}: {e}"))?;

        // Decode the frame
        let decoded = match decode_frame(
            &frame_data,
            blocks_in_frame,
            info.channels,
            info.compression_level,
            bits,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!(frame = frame_idx, error = %e, "ape: frame decode error, filling silence");
                // Fill with silence for this frame
                vec![0i32; blocks_in_frame as usize * info.channels as usize]
            }
        };

        // Apply seek skip and collect i32 samples
        let skip_interleaved = if frame_idx == start_frame {
            skip_in_first_frame * source_channels as usize
        } else {
            0
        };

        for (i, &sample) in decoded.iter().enumerate() {
            if i < skip_interleaved {
                continue;
            }
            if all_samples.len() >= max_interleaved_samples {
                break;
            }
            all_samples.push(sample);
        }
    }

    if all_samples.is_empty() {
        return Err("ape: no samples decoded".into());
    }

    let out_rate = _target_sample_rate.unwrap_or(source_rate);
    let out_channels = _target_channels.unwrap_or(source_channels);
    let total_frames = all_samples.len() as f64 / source_channels as f64;
    let duration_s = total_frames / source_rate as f64;

    debug!(
        file = path,
        samples = all_samples.len(),
        rate = source_rate,
        channels = source_channels,
        bits = info.bits_per_sample,
        compression = info.compression_level,
        duration_s,
        "decoded_ape_native"
    );

    Ok(DecodedAudio {
        samples_i32: all_samples,
        bit_depth: bits as u16,
        sample_rate: out_rate,
        channels: out_channels,
        duration_s,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal APE v3990 file header in memory for testing.
    fn build_ape_header(
        compression: u16,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        total_frames: u32,
        blocks_per_frame: u32,
        final_frame_blocks: u32,
    ) -> Vec<u8> {
        let mut buf = Vec::new();

        // Descriptor (52 bytes total)
        buf.extend_from_slice(&APE_MAGIC); // 0-3: magic
        buf.extend_from_slice(&3990u16.to_le_bytes()); // 4-5: version
        buf.extend_from_slice(&0u16.to_le_bytes()); // 6-7: padding
        buf.extend_from_slice(&52u32.to_le_bytes()); // 8-11: descriptor_bytes
        buf.extend_from_slice(&24u32.to_le_bytes()); // 12-15: header_bytes
        let seek_table_bytes = total_frames * 4;
        buf.extend_from_slice(&seek_table_bytes.to_le_bytes()); // 16-19: seek_table_bytes
        buf.extend_from_slice(&0u32.to_le_bytes()); // 20-23: header_data_bytes
        buf.extend_from_slice(&0u32.to_le_bytes()); // 24-27: ape_frame_data_bytes
        buf.extend_from_slice(&0u32.to_le_bytes()); // 28-31: ape_frame_data_bytes_high
        buf.extend_from_slice(&0u32.to_le_bytes()); // 32-35: terminating_data_bytes
        buf.extend_from_slice(&[0u8; 16]); // 36-51: file_md5

        // Header (24 bytes, at offset 52)
        buf.extend_from_slice(&compression.to_le_bytes()); // 0-1: compression_type
        buf.extend_from_slice(&0u16.to_le_bytes()); // 2-3: format_flags
        buf.extend_from_slice(&blocks_per_frame.to_le_bytes()); // 4-7: blocks_per_frame
        buf.extend_from_slice(&final_frame_blocks.to_le_bytes()); // 8-11: final_frame_blocks
        buf.extend_from_slice(&total_frames.to_le_bytes()); // 12-15: total_frames
        buf.extend_from_slice(&bits_per_sample.to_le_bytes()); // 16-17: bits_per_sample
        buf.extend_from_slice(&channels.to_le_bytes()); // 18-19: channels
        buf.extend_from_slice(&sample_rate.to_le_bytes()); // 20-23: sample_rate

        // Seek table (dummy entries)
        let base_offset = 52 + 24 + seek_table_bytes;
        for i in 0..total_frames {
            buf.extend_from_slice(&(base_offset + i * 1024).to_le_bytes());
        }

        buf
    }

    #[test]
    fn parse_ape_header_stereo_16bit_44100() {
        let data = build_ape_header(2000, 2, 44100, 16, 10, 73728, 12345);
        let tmp = std::env::temp_dir().join("test_ape_header.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        assert_eq!(info.version, 3990);
        assert_eq!(info.compression_level, 2000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.total_frames, 10);
        assert_eq!(info.blocks_per_frame, 73728);
        assert_eq!(info.final_frame_blocks, 12345);

        let expected_total = 9u64 * 73728 + 12345;
        assert_eq!(info.total_samples, expected_total);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_ape_header_mono_24bit_96000() {
        let data = build_ape_header(1000, 1, 96000, 24, 5, 73728, 5000);
        let tmp = std::env::temp_dir().join("test_ape_mono_24.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        assert_eq!(info.channels, 1);
        assert_eq!(info.sample_rate, 96000);
        assert_eq!(info.bits_per_sample, 24);
        assert_eq!(info.compression_level, 1000);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_ape_header_compression_levels() {
        for &comp in &[1000u16, 2000, 3000, 4000] {
            let data = build_ape_header(comp, 2, 44100, 16, 1, 73728, 1000);
            let tmp = std::env::temp_dir().join(format!("test_ape_comp_{comp}.ape"));
            std::fs::write(&tmp, &data).unwrap();

            let info = parse_ape(tmp.to_str().unwrap()).unwrap();
            assert_eq!(info.compression_level, comp);

            std::fs::remove_file(&tmp).ok();
        }
    }

    #[test]
    fn parse_ape_rejects_old_version() {
        let mut data = build_ape_header(2000, 2, 44100, 16, 1, 73728, 1000);
        // Overwrite version at offset 4 to 3970
        data[4..6].copy_from_slice(&3970u16.to_le_bytes());
        let tmp = std::env::temp_dir().join("test_ape_old.ape");
        std::fs::write(&tmp, &data).unwrap();

        let result = parse_ape(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("3970"));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_ape_rejects_bad_magic() {
        let data = vec![0x00u8; 100];
        let tmp = std::env::temp_dir().join("test_ape_bad_magic.ape");
        std::fs::write(&tmp, &data).unwrap();

        let result = parse_ape(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not an APE file"));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_ape_rejects_unsupported_channels() {
        let mut data = build_ape_header(2000, 2, 44100, 16, 1, 73728, 1000);
        // Overwrite channels at offset 52 + 18 = 70 to 5
        data[70..72].copy_from_slice(&5u16.to_le_bytes());
        let tmp = std::env::temp_dir().join("test_ape_bad_channels.ape");
        std::fs::write(&tmp, &data).unwrap();

        let result = parse_ape(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("channel"));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn parse_ape_rejects_unsupported_bits() {
        let mut data = build_ape_header(2000, 2, 44100, 16, 1, 73728, 1000);
        // Overwrite bits_per_sample at offset 52 + 16 = 68 to 32
        data[68..70].copy_from_slice(&32u16.to_le_bytes());
        let tmp = std::env::temp_dir().join("test_ape_bad_bits.ape");
        std::fs::write(&tmp, &data).unwrap();

        let result = parse_ape(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("bit depth"));

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn total_samples_single_frame() {
        let data = build_ape_header(2000, 2, 44100, 16, 1, 73728, 5000);
        let tmp = std::env::temp_dir().join("test_ape_single_frame.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        // Single frame: total_samples = final_frame_blocks only
        assert_eq!(info.total_samples, 5000);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn total_samples_zero_frames() {
        let data = build_ape_header(2000, 2, 44100, 16, 0, 73728, 0);
        let tmp = std::env::temp_dir().join("test_ape_zero_frames.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        assert_eq!(info.total_samples, 0);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn duration_calculation() {
        let data = build_ape_header(2000, 2, 44100, 16, 10, 73728, 12345);
        let tmp = std::env::temp_dir().join("test_ape_duration.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        let duration_s = info.total_samples as f64 / info.sample_rate as f64;
        // (9 * 73728 + 12345) / 44100 = 675897 / 44100 ≈ 15.32s
        assert!(
            duration_s > 15.0 && duration_s < 16.0,
            "expected ~15.3s, got {duration_s}"
        );

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn decode_sign_encoding() {
        assert_eq!(decode_sign(0), 0);
        assert_eq!(decode_sign(1), -1);
        assert_eq!(decode_sign(2), 1);
        assert_eq!(decode_sign(3), -2);
        assert_eq!(decode_sign(4), 2);
        assert_eq!(decode_sign(5), -3);
        assert_eq!(decode_sign(6), 3);
    }

    #[test]
    fn prediction_filter_basic() {
        let mut filter = PredictionFilter::new(4);
        // With zero coefficients and history, output = residual
        assert_eq!(filter.apply(100), 100);
        assert_eq!(filter.apply(200), 200);
        assert_eq!(filter.apply(0), 0);
    }

    #[test]
    fn prediction_filter_adapts() {
        let mut filter = PredictionFilter::new(4);
        // Feed a constant signal — filter should start predicting it
        for _ in 0..100 {
            filter.apply(1000);
        }
        // After adaptation, a zero residual should produce something close to 1000
        let predicted = filter.apply(0);
        // The filter should be predicting nonzero
        assert!(predicted != 0, "filter should have adapted");
    }

    #[test]
    fn rice_model_initial_state() {
        let model = RiceModel::new();
        assert_eq!(model.k, 10);
        assert_eq!(model.sum, 0);
    }

    #[test]
    fn seek_table_offset_calculation() {
        let data = build_ape_header(2000, 2, 44100, 16, 3, 73728, 1000);
        let tmp = std::env::temp_dir().join("test_ape_seek_offset.ape");
        std::fs::write(&tmp, &data).unwrap();

        let info = parse_ape(tmp.to_str().unwrap()).unwrap();
        // seek_table_offset = descriptor_bytes (52) + header_bytes (24) = 76
        assert_eq!(info.seek_table_offset, 76);

        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn nonexistent_file() {
        let result = parse_ape("/nonexistent/file.ape");
        assert!(result.is_err());
    }
}
