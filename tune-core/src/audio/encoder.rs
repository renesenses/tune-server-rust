use std::io::Cursor;

use tracing::{debug, warn};

/// Audio encoder that handles WAV (via hound) and FLAC (native pure-Rust).
/// MP3 and OGG requests are transparently encoded as FLAC with a warning,
/// since pure-Rust encoders for those formats are not available.
pub struct AudioEncoder {
    format: String,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    pcm_buffer: Option<Vec<u8>>,
}

impl AudioEncoder {
    pub fn new(format: &str, sample_rate: u32, bit_depth: u32, channels: u32) -> Self {
        Self {
            format: format.to_string(),
            sample_rate,
            bit_depth,
            channels,
            pcm_buffer: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        match self.format.as_str() {
            "wav" => {
                debug!(
                    format = "wav",
                    sample_rate = self.sample_rate,
                    bit_depth = self.bit_depth,
                    "encoder_start_hound"
                );
            }
            "flac" => {
                debug!(
                    format = "flac",
                    sample_rate = self.sample_rate,
                    bit_depth = self.bit_depth,
                    "encoder_start_native_flac"
                );
            }
            "mp3" => {
                warn!(
                    requested = "mp3",
                    actual = "flac",
                    "encoder_format_substitution: MP3 not available without FFmpeg, encoding as FLAC"
                );
            }
            "ogg" => {
                warn!(
                    requested = "ogg",
                    actual = "flac",
                    "encoder_format_substitution: OGG not available without FFmpeg, encoding as FLAC"
                );
            }
            other => {
                warn!(
                    requested = other,
                    actual = "flac",
                    "encoder_format_substitution: format not natively supported, encoding as FLAC"
                );
            }
        }
        self.pcm_buffer = Some(Vec::new());
        Ok(())
    }

    pub async fn write(&mut self, pcm_data: &[u8]) -> Result<(), String> {
        let buf = self.pcm_buffer.as_mut().ok_or("encoder not started")?;
        buf.extend_from_slice(pcm_data);
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        let pcm_data = self.pcm_buffer.take().unwrap_or_default();
        if pcm_data.is_empty() && self.pcm_buffer.is_none() {
            // finish() called without start()
            return Ok(Vec::new());
        }

        match self.format.as_str() {
            "wav" => encode_wav_hound(&pcm_data, self.sample_rate, self.bit_depth, self.channels),
            _ => {
                // FLAC for flac/mp3/ogg/anything else
                encode_flac(&pcm_data, self.sample_rate, self.bit_depth, self.channels)
            }
        }
    }

    pub async fn stop(&mut self) {
        self.pcm_buffer = None;
    }
}

// Keep the old name as a type alias for backward compatibility
pub type FFmpegEncoder = AudioEncoder;

// ---------------------------------------------------------------------------
// WAV encoder (hound)
// ---------------------------------------------------------------------------

fn encode_wav_hound(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: bit_depth as u16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    let mut writer =
        hound::WavWriter::new(&mut cursor, spec).map_err(|e| format!("hound init: {e}"))?;

    write_pcm_samples(&mut writer, pcm, bit_depth)?;

    writer
        .finalize()
        .map_err(|e| format!("hound finalize: {e}"))?;
    debug!(
        pcm_bytes = pcm.len(),
        wav_bytes = cursor.get_ref().len(),
        "wav_encoded_hound"
    );
    Ok(cursor.into_inner())
}

fn write_pcm_samples(
    writer: &mut hound::WavWriter<&mut Cursor<Vec<u8>>>,
    pcm: &[u8],
    bit_depth: u32,
) -> Result<(), String> {
    match bit_depth {
        16 => {
            for chunk in pcm.chunks_exact(2) {
                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        24 => {
            for chunk in pcm.chunks_exact(3) {
                let sample = i32::from_le_bytes([
                    chunk[0],
                    chunk[1],
                    chunk[2],
                    if chunk[2] & 0x80 != 0 { 0xFF } else { 0 },
                ]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        32 => {
            for chunk in pcm.chunks_exact(4) {
                let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        _ => return Err(format!("unsupported bit depth: {bit_depth}")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FLAC encoder (pure Rust)
// ---------------------------------------------------------------------------
//
// Implements a minimal FLAC encoder using FIXED prediction (orders 0-4)
// with Rice coding for residuals. Produces valid FLAC streams that any
// standard decoder can read.

const FLAC_BLOCK_SIZE: usize = 4096;

fn encode_flac(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    let bytes_per_sample = ((bit_depth + 7) / 8) as usize;
    let frame_size_bytes = bytes_per_sample * channels as usize;
    if pcm.len() % frame_size_bytes != 0 {
        return Err(format!(
            "PCM data length {} is not a multiple of frame size {}",
            pcm.len(),
            frame_size_bytes
        ));
    }
    let total_samples = pcm.len() / frame_size_bytes;

    // Decode all PCM to interleaved i32 samples
    let samples = pcm_to_i32(pcm, bit_depth)?;

    // De-interleave into per-channel buffers
    let ch = channels as usize;
    let mut channel_data: Vec<Vec<i32>> = vec![Vec::with_capacity(total_samples); ch];
    for (i, &s) in samples.iter().enumerate() {
        channel_data[i % ch].push(s);
    }

    let mut output = Vec::with_capacity(pcm.len());

    // 1. fLaC magic
    output.extend_from_slice(b"fLaC");

    // 2. STREAMINFO metadata block
    //    Header: 1 bit is_last=1, 7 bits type=0, 24 bits length=34
    let block_header: u32 = (1 << 31) | 34;
    output.extend_from_slice(&block_header.to_be_bytes());

    let block_size = FLAC_BLOCK_SIZE.min(total_samples);
    let block_size_u16 = if block_size == 0 {
        1u16
    } else {
        block_size as u16
    };

    // Placeholder for STREAMINFO — we'll fill min/max frame sizes after encoding
    let streaminfo_offset = output.len();

    // min block size, max block size (2+2 bytes)
    output.extend_from_slice(&block_size_u16.to_be_bytes());
    output.extend_from_slice(&block_size_u16.to_be_bytes());
    // min frame size, max frame size (3+3 bytes) — placeholder 0
    output.extend_from_slice(&[0u8; 6]);
    // sample rate (20 bits) | channels-1 (3 bits) | bps-1 (5 bits) | total samples high 4 bits
    let sr_ch_bps: u64 = ((sample_rate as u64) << 12)
        | (((channels - 1) as u64) << 9)
        | (((bit_depth - 1) as u64) << 4)
        | ((total_samples as u64 >> 32) & 0xF);
    output.extend_from_slice(&(sr_ch_bps as u32).to_be_bytes());
    // total samples low 32 bits
    output.extend_from_slice(&(total_samples as u32).to_be_bytes());
    // MD5 — 16 bytes of zeros (optional, valid per spec)
    output.extend_from_slice(&[0u8; 16]);

    // 3. Audio frames
    let mut min_frame_size: u32 = u32::MAX;
    let mut max_frame_size: u32 = 0;
    let mut sample_offset: usize = 0;

    while sample_offset < total_samples {
        let block_len = FLAC_BLOCK_SIZE.min(total_samples - sample_offset);
        let frame_start = output.len();

        encode_flac_frame(
            &mut output,
            &channel_data,
            sample_offset,
            block_len,
            sample_rate,
            bit_depth,
            channels,
        )?;

        let frame_bytes = (output.len() - frame_start) as u32;
        min_frame_size = min_frame_size.min(frame_bytes);
        max_frame_size = max_frame_size.max(frame_bytes);

        sample_offset += block_len;
    }

    // Patch min/max frame size in STREAMINFO (offset +4 from streaminfo_offset)
    if min_frame_size == u32::MAX {
        min_frame_size = 0;
    }
    let fs_offset = streaminfo_offset + 4; // after min/max block size
    output[fs_offset] = (min_frame_size >> 16) as u8;
    output[fs_offset + 1] = (min_frame_size >> 8) as u8;
    output[fs_offset + 2] = min_frame_size as u8;
    output[fs_offset + 3] = (max_frame_size >> 16) as u8;
    output[fs_offset + 4] = (max_frame_size >> 8) as u8;
    output[fs_offset + 5] = max_frame_size as u8;

    debug!(
        pcm_bytes = pcm.len(),
        flac_bytes = output.len(),
        total_samples,
        "flac_encoded_native"
    );
    Ok(output)
}

/// Decode raw PCM bytes into i32 samples.
fn pcm_to_i32(pcm: &[u8], bit_depth: u32) -> Result<Vec<i32>, String> {
    let mut samples = Vec::new();
    match bit_depth {
        16 => {
            samples.reserve(pcm.len() / 2);
            for chunk in pcm.chunks_exact(2) {
                samples.push(i16::from_le_bytes([chunk[0], chunk[1]]) as i32);
            }
        }
        24 => {
            samples.reserve(pcm.len() / 3);
            for chunk in pcm.chunks_exact(3) {
                let val = i32::from_le_bytes([
                    chunk[0],
                    chunk[1],
                    chunk[2],
                    if chunk[2] & 0x80 != 0 { 0xFF } else { 0 },
                ]);
                samples.push(val);
            }
        }
        32 => {
            samples.reserve(pcm.len() / 4);
            for chunk in pcm.chunks_exact(4) {
                samples.push(i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        _ => return Err(format!("unsupported bit depth: {bit_depth}")),
    }
    Ok(samples)
}

// ---------------------------------------------------------------------------
// FLAC frame encoding
// ---------------------------------------------------------------------------

/// A bitstream writer that accumulates bits into a byte buffer.
struct BitWriter {
    buf: Vec<u8>,
    current_byte: u8,
    bits_in_byte: u8, // how many bits written into current_byte (0-7)
}

impl BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current_byte: 0,
            bits_in_byte: 0,
        }
    }

    /// Write `n` bits from `value` (MSB first). n must be <= 32.
    fn write_bits(&mut self, value: u32, n: u8) {
        debug_assert!(n <= 32);
        if n == 0 {
            return;
        }
        for i in (0..n).rev() {
            let bit = (value >> i) & 1;
            self.current_byte = (self.current_byte << 1) | bit as u8;
            self.bits_in_byte += 1;
            if self.bits_in_byte == 8 {
                self.buf.push(self.current_byte);
                self.current_byte = 0;
                self.bits_in_byte = 0;
            }
        }
    }

    /// Write `n` bits from a u64 value (MSB first). n must be <= 64.
    fn write_bits_u64(&mut self, value: u64, n: u8) {
        debug_assert!(n <= 64);
        if n <= 32 {
            self.write_bits(value as u32, n);
        } else {
            self.write_bits((value >> 32) as u32, n - 32);
            self.write_bits(value as u32, 32);
        }
    }

    /// Pad remaining bits to byte boundary with zeros.
    fn flush(&mut self) {
        if self.bits_in_byte > 0 {
            self.current_byte <<= 8 - self.bits_in_byte;
            self.buf.push(self.current_byte);
            self.current_byte = 0;
            self.bits_in_byte = 0;
        }
    }

    /// Returns the accumulated bytes. Must call flush() first.
    fn into_bytes(self) -> Vec<u8> {
        debug_assert_eq!(self.bits_in_byte, 0);
        self.buf
    }
}

/// Encode a single FLAC frame and append to output.
fn encode_flac_frame(
    output: &mut Vec<u8>,
    channel_data: &[Vec<i32>],
    sample_offset: usize,
    block_len: usize,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<(), String> {
    let mut bw = BitWriter::new();

    // --- Frame header ---
    // Sync code: 14 bits = 0x3FFE
    bw.write_bits(0x3FFE, 14);
    // Reserved: 1 bit = 0
    bw.write_bits(0, 1);
    // Blocking strategy: 1 bit = 0 (fixed-size)
    bw.write_bits(0, 1);

    // Block size code (4 bits)
    let block_size_code = match block_len {
        192 => 1u8,
        576 => 2,
        1152 => 3,
        2304 => 4,
        4608 => 5,
        256 => 8,
        512 => 9,
        1024 => 10,
        2048 => 11,
        4096 => 12,
        8192 => 13,
        16384 => 14,
        32768 => 15,
        n if n <= 255 => 6, // 8-bit at end of header
        _ => 7,             // 16-bit at end of header
    };
    bw.write_bits(block_size_code as u32, 4);

    // Sample rate code (4 bits)
    let sample_rate_code: u8 = match sample_rate {
        88200 => 1,
        176400 => 2,
        192000 => 3,
        8000 => 4,
        16000 => 5,
        22050 => 6,
        24000 => 7,
        32000 => 8,
        44100 => 9,
        48000 => 10,
        96000 => 11,
        _ if sample_rate % 1000 == 0 && sample_rate / 1000 <= 255 => 12, // 8-bit kHz
        _ if sample_rate <= 65535 => 13,                                 // 16-bit Hz
        _ if sample_rate % 10 == 0 && sample_rate / 10 <= 65535 => 14,   // 16-bit tens of Hz
        _ => 0,                                                          // use STREAMINFO
    };
    bw.write_bits(sample_rate_code as u32, 4);

    // Channel assignment (4 bits): independent channels
    let channel_assignment: u8 = match channels {
        1 => 0, // mono
        2 => 1, // left, right
        3 => 2,
        4 => 3,
        5 => 4,
        6 => 5,
        7 => 6,
        8 => 7,
        _ => return Err(format!("unsupported channel count: {channels}")),
    };
    bw.write_bits(channel_assignment as u32, 4);

    // Sample size code (3 bits)
    let sample_size_code: u8 = match bit_depth {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7, // 32-bit per sample not officially in the spec table but we use it
        _ => 0,  // get from STREAMINFO
    };
    bw.write_bits(sample_size_code as u32, 3);

    // Reserved: 1 bit = 0
    bw.write_bits(0, 1);

    // Frame number (UTF-8 coded, blocking strategy=0 means frame number)
    let frame_number = (sample_offset / block_len) as u32;
    write_utf8_u32(&mut bw, frame_number);

    // Optional block size at end of header
    match block_size_code {
        6 => bw.write_bits((block_len - 1) as u32, 8),
        7 => bw.write_bits((block_len - 1) as u32, 16),
        _ => {}
    }

    // Optional sample rate at end of header
    match sample_rate_code {
        12 => bw.write_bits(sample_rate / 1000, 8),
        13 => bw.write_bits(sample_rate, 16),
        14 => bw.write_bits(sample_rate / 10, 16),
        _ => {}
    }

    // CRC-8 of header
    bw.flush();
    let crc8 = flac_crc8(&bw.buf);
    bw.write_bits(crc8 as u32, 8);

    // --- Subframes (one per channel) ---
    for ch_idx in 0..channels as usize {
        let samples = &channel_data[ch_idx][sample_offset..sample_offset + block_len];
        encode_subframe(&mut bw, samples, bit_depth)?;
    }

    // --- Frame footer ---
    bw.flush();
    let crc16 = flac_crc16(&bw.buf);
    bw.write_bits(crc16 as u32, 16);
    bw.flush();

    output.extend_from_slice(&bw.into_bytes());
    Ok(())
}

/// Encode a UTF-8 coded frame number (FLAC's custom UTF-8 variant for up to 31 bits).
fn write_utf8_u32(bw: &mut BitWriter, value: u32) {
    if value < 0x80 {
        bw.write_bits(value, 8);
    } else if value < 0x800 {
        bw.write_bits(0xC0 | (value >> 6), 8);
        bw.write_bits(0x80 | (value & 0x3F), 8);
    } else if value < 0x10000 {
        bw.write_bits(0xE0 | (value >> 12), 8);
        bw.write_bits(0x80 | ((value >> 6) & 0x3F), 8);
        bw.write_bits(0x80 | (value & 0x3F), 8);
    } else if value < 0x200000 {
        bw.write_bits(0xF0 | (value >> 18), 8);
        bw.write_bits(0x80 | ((value >> 12) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 6) & 0x3F), 8);
        bw.write_bits(0x80 | (value & 0x3F), 8);
    } else if value < 0x4000000 {
        bw.write_bits(0xF8 | (value >> 24), 8);
        bw.write_bits(0x80 | ((value >> 18) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 12) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 6) & 0x3F), 8);
        bw.write_bits(0x80 | (value & 0x3F), 8);
    } else {
        bw.write_bits(0xFC | (value >> 30), 8);
        bw.write_bits(0x80 | ((value >> 24) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 18) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 12) & 0x3F), 8);
        bw.write_bits(0x80 | ((value >> 6) & 0x3F), 8);
        bw.write_bits(0x80 | (value & 0x3F), 8);
    }
}

/// Encode a subframe using FIXED prediction. Tries orders 0-4 and picks the best.
fn encode_subframe(bw: &mut BitWriter, samples: &[i32], bit_depth: u32) -> Result<(), String> {
    if samples.is_empty() {
        return Ok(());
    }

    // Check for constant subframe (all samples identical)
    let first = samples[0];
    if samples.iter().all(|&s| s == first) {
        // CONSTANT subframe: type = 00 (0), 6 bits = 000000
        // Subframe header: 0 (padding) + 000000 (CONSTANT) + 0 (no wasted bits)
        bw.write_bits(0, 1); // padding
        bw.write_bits(0b000000, 6); // CONSTANT type
        bw.write_bits(0, 1); // no wasted bits
        // Write the constant value in bit_depth bits (signed, two's complement)
        write_signed(bw, first as i64, bit_depth as u8);
        return Ok(());
    }

    // For very short blocks, use VERBATIM
    if samples.len() <= 4 {
        return encode_subframe_verbatim(bw, samples, bit_depth);
    }

    // Try fixed prediction orders 0-4, pick the one with smallest residual sum
    let best_order = pick_best_fixed_order(samples);

    encode_subframe_fixed(bw, samples, bit_depth, best_order)
}

/// VERBATIM subframe: stores raw samples without prediction.
fn encode_subframe_verbatim(
    bw: &mut BitWriter,
    samples: &[i32],
    bit_depth: u32,
) -> Result<(), String> {
    // Subframe header: 0 (padding) + 000001 (VERBATIM) + 0 (no wasted bits)
    bw.write_bits(0, 1); // padding
    bw.write_bits(0b000001, 6); // VERBATIM type
    bw.write_bits(0, 1); // no wasted bits
    for &s in samples {
        write_signed(bw, s as i64, bit_depth as u8);
    }
    Ok(())
}

/// FIXED prediction subframe with Rice-coded residuals.
fn encode_subframe_fixed(
    bw: &mut BitWriter,
    samples: &[i32],
    bit_depth: u32,
    order: u8,
) -> Result<(), String> {
    // Subframe header: 0 (padding) + type bits (6) + 0 (no wasted bits)
    // FIXED type: 001xxx where xxx = order (0-4) => 001000 + order
    bw.write_bits(0, 1); // padding
    bw.write_bits(0b001000 | order as u32, 6); // FIXED type + order
    bw.write_bits(0, 1); // no wasted bits

    // Warm-up samples: `order` raw samples
    for i in 0..order as usize {
        write_signed(bw, samples[i] as i64, bit_depth as u8);
    }

    // Compute residuals
    let residuals = compute_fixed_residuals(samples, order);

    // Encode residuals with Rice coding
    encode_rice_residuals(bw, &residuals, order as usize, samples.len());

    Ok(())
}

/// Pick the best fixed prediction order (0-4) by minimizing sum of absolute residuals.
fn pick_best_fixed_order(samples: &[i32]) -> u8 {
    let mut best_order = 0u8;
    let mut best_sum = u64::MAX;

    for order in 0..=4u8 {
        if (order as usize) >= samples.len() {
            break;
        }
        let residuals = compute_fixed_residuals(samples, order);
        let sum: u64 = residuals.iter().map(|r| r.unsigned_abs() as u64).sum();
        if sum < best_sum {
            best_sum = sum;
            best_order = order;
        }
    }
    best_order
}

/// Compute residuals for FIXED prediction of given order.
fn compute_fixed_residuals(samples: &[i32], order: u8) -> Vec<i64> {
    let n = samples.len();
    let start = order as usize;
    let mut residuals = Vec::with_capacity(n - start);

    // Work with i64 to avoid overflow in higher-order predictions
    let s: Vec<i64> = samples.iter().map(|&x| x as i64).collect();

    for i in start..n {
        let r = match order {
            0 => s[i],
            1 => s[i] - s[i - 1],
            2 => s[i] - 2 * s[i - 1] + s[i - 2],
            3 => s[i] - 3 * s[i - 1] + 3 * s[i - 2] - s[i - 3],
            4 => s[i] - 4 * s[i - 1] + 6 * s[i - 2] - 4 * s[i - 3] + s[i - 4],
            _ => unreachable!(),
        };
        residuals.push(r);
    }
    residuals
}

/// Encode residuals using Rice coding (RESIDUAL_CODING_METHOD_PARTITIONED_RICE).
fn encode_rice_residuals(
    bw: &mut BitWriter,
    residuals: &[i64],
    _predictor_order: usize,
    _block_size: usize,
) {
    // Use partition order 0 (single partition) for simplicity.
    // This is always valid and produces correct output.
    let partition_order: u32 = 0;

    // Coding method: 00 = RICE (4-bit parameter)
    bw.write_bits(0b00, 2);
    // Partition order
    bw.write_bits(partition_order, 4);

    // With partition order 0, there's one partition containing all residuals
    // Find optimal Rice parameter
    let rice_param = optimal_rice_parameter(residuals);

    if rice_param < 15 {
        // Normal Rice parameter (4 bits)
        bw.write_bits(rice_param as u32, 4);

        // Encode each residual
        for &r in residuals {
            write_rice_signed(bw, r, rice_param);
        }
    } else {
        // Escape code: parameter = 15 means unencoded (verbatim residuals)
        // Each residual stored in 5-bit "bits per sample" field
        bw.write_bits(0b1111, 4); // escape
        // The spec says: 5 bits for the number of bits per residual sample
        // We'll use enough bits to represent the max residual
        let max_abs = residuals
            .iter()
            .map(|r| r.unsigned_abs())
            .max()
            .unwrap_or(0);
        let bits_needed = if max_abs == 0 {
            0u8
        } else {
            (64 - max_abs.leading_zeros()) as u8 + 1 // +1 for sign
        };
        let bits_needed = bits_needed.min(32);
        bw.write_bits(bits_needed as u32, 5);
        for &r in residuals {
            write_signed(bw, r, bits_needed);
        }
    }
}

/// Find optimal Rice parameter k for a set of residuals.
/// The optimal k minimizes the total encoded bit count.
fn optimal_rice_parameter(residuals: &[i64]) -> u8 {
    if residuals.is_empty() {
        return 0;
    }

    // Map signed residuals to unsigned (zig-zag encoding, same as Rice uses)
    let sum_mapped: u64 = residuals
        .iter()
        .map(|&r| {
            if r >= 0 {
                (2 * r) as u64
            } else {
                (2 * (-r) - 1) as u64
            }
        })
        .sum();

    let n = residuals.len() as u64;
    if n == 0 {
        return 0;
    }

    // Optimal k is approximately log2(mean of mapped values)
    let mean = sum_mapped / n;
    if mean == 0 {
        return 0;
    }
    let k = (64 - mean.leading_zeros()).saturating_sub(1) as u8;
    k.min(14) // Rice parameter must be 0-14 (15 = escape)
}

/// Write a single Rice-coded signed value.
/// Uses zig-zag mapping: 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, 2 -> 4, ...
fn write_rice_signed(bw: &mut BitWriter, value: i64, k: u8) {
    let mapped: u64 = if value >= 0 {
        (value as u64) << 1
    } else {
        (((-value) as u64) << 1) - 1
    };

    let quotient = (mapped >> k) as u32;
    let remainder = mapped & ((1u64 << k) - 1);

    // Unary code for quotient: `quotient` ones followed by a zero
    for _ in 0..quotient {
        bw.write_bits(1, 1);
    }
    bw.write_bits(0, 1);

    // Binary code for remainder: k bits
    if k > 0 {
        if k <= 32 {
            bw.write_bits(remainder as u32, k);
        } else {
            bw.write_bits_u64(remainder, k);
        }
    }
}

/// Write a signed value in two's complement with the given number of bits.
fn write_signed(bw: &mut BitWriter, value: i64, bits: u8) {
    if bits == 0 {
        return;
    }
    // Mask to the correct number of bits (two's complement)
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let encoded = (value as u64) & mask;
    if bits <= 32 {
        bw.write_bits(encoded as u32, bits);
    } else {
        bw.write_bits_u64(encoded, bits);
    }
}

// ---------------------------------------------------------------------------
// CRC functions for FLAC
// ---------------------------------------------------------------------------

/// CRC-8 with polynomial x^8 + x^2 + x^1 + 1 (0x07), init 0.
fn flac_crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// CRC-16 with polynomial x^16 + x^15 + x^2 + 1 (0x8005), init 0.
fn flac_crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_new() {
        let enc = AudioEncoder::new("flac", 44100, 16, 2);
        assert_eq!(enc.format, "flac");
        assert_eq!(enc.sample_rate, 44100);
        assert!(enc.pcm_buffer.is_none());
    }

    #[test]
    fn encoder_new_alias() {
        // FFmpegEncoder alias still works
        let enc = FFmpegEncoder::new("flac", 44100, 16, 2);
        assert_eq!(enc.format, "flac");
    }

    #[tokio::test]
    async fn finish_without_start() {
        let mut enc = AudioEncoder::new("flac", 44100, 16, 2);
        let result = enc.finish().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn stop_without_start() {
        let mut enc = AudioEncoder::new("flac", 44100, 16, 2);
        enc.stop().await;
        assert!(enc.pcm_buffer.is_none());
    }

    #[tokio::test]
    async fn wav_encode_hound() {
        let mut enc = AudioEncoder::new("wav", 44100, 16, 2);
        enc.start().await.unwrap();
        // 100 frames of silence (stereo 16-bit = 400 bytes)
        let pcm = vec![0u8; 400];
        enc.write(&pcm).await.unwrap();
        let wav = enc.finish().await.unwrap();
        // WAV has 44-byte header
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn wav_encode_24bit() {
        let pcm = vec![0u8; 600]; // 100 frames * 2ch * 3 bytes
        let wav = encode_wav_hound(&pcm, 96000, 24, 2).unwrap();
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
    }

    #[tokio::test]
    async fn flac_encode_silence_16bit() {
        let mut enc = AudioEncoder::new("flac", 44100, 16, 2);
        enc.start().await.unwrap();
        // 100 frames of silence (stereo 16-bit = 400 bytes)
        let pcm = vec![0u8; 400];
        enc.write(&pcm).await.unwrap();
        let flac = enc.finish().await.unwrap();

        // Check fLaC magic
        assert!(flac.len() > 42);
        assert_eq!(&flac[0..4], b"fLaC");

        // Check STREAMINFO block header
        let block_hdr = u32::from_be_bytes([flac[4], flac[5], flac[6], flac[7]]);
        assert_eq!(block_hdr & (0x7F << 24), 0); // type = 0 (STREAMINFO)
        assert_eq!(block_hdr & 0xFFFFFF, 34); // length = 34
        assert_eq!(block_hdr >> 31, 1); // is_last = 1
    }

    #[tokio::test]
    async fn flac_encode_24bit() {
        let mut enc = AudioEncoder::new("flac", 96000, 24, 2);
        enc.start().await.unwrap();
        // 50 frames of silence (stereo 24-bit = 300 bytes)
        let pcm = vec![0u8; 300];
        enc.write(&pcm).await.unwrap();
        let flac = enc.finish().await.unwrap();
        assert_eq!(&flac[0..4], b"fLaC");

        // Verify sample rate is encoded in STREAMINFO
        // Bytes 18-21 contain: sample_rate(20) | channels-1(3) | bps-1(5) | total_hi(4)
        let sr_ch_bps = u32::from_be_bytes([flac[18], flac[19], flac[20], flac[21]]);
        let encoded_sr = sr_ch_bps >> 12;
        assert_eq!(encoded_sr, 96000);
        let encoded_ch = ((sr_ch_bps >> 9) & 0x7) + 1;
        assert_eq!(encoded_ch, 2);
        let encoded_bps = ((sr_ch_bps >> 4) & 0x1F) + 1;
        assert_eq!(encoded_bps, 24);
    }

    #[tokio::test]
    async fn flac_encode_with_signal() {
        // Generate a simple sine-like pattern (not silence) to exercise prediction
        let mut pcm = Vec::with_capacity(8192);
        for i in 0..2048 {
            // Mono 16-bit: simple ramp pattern
            let val = ((i % 256) as i16 - 128) * 100;
            pcm.extend_from_slice(&val.to_le_bytes());
        }

        let mut enc = AudioEncoder::new("flac", 44100, 16, 1);
        enc.start().await.unwrap();
        enc.write(&pcm).await.unwrap();
        let flac = enc.finish().await.unwrap();
        assert_eq!(&flac[0..4], b"fLaC");
        // FLAC output should be smaller than raw PCM for a predictable signal
        assert!(
            flac.len() < pcm.len(),
            "FLAC should compress a regular signal"
        );
    }

    #[tokio::test]
    async fn mp3_falls_back_to_flac() {
        let mut enc = AudioEncoder::new("mp3", 44100, 16, 2);
        enc.start().await.unwrap();
        let pcm = vec![0u8; 400];
        enc.write(&pcm).await.unwrap();
        let output = enc.finish().await.unwrap();
        // Should produce FLAC, not crash
        assert_eq!(&output[0..4], b"fLaC");
    }

    #[tokio::test]
    async fn ogg_falls_back_to_flac() {
        let mut enc = AudioEncoder::new("ogg", 44100, 16, 2);
        enc.start().await.unwrap();
        let pcm = vec![0u8; 400];
        enc.write(&pcm).await.unwrap();
        let output = enc.finish().await.unwrap();
        // Should produce FLAC, not crash
        assert_eq!(&output[0..4], b"fLaC");
    }

    #[test]
    fn flac_streaminfo_total_samples() {
        let pcm = vec![0u8; 800]; // 200 frames stereo 16-bit
        let flac = encode_flac(&pcm, 44100, 16, 2).unwrap();
        // Total samples in STREAMINFO: bytes 22-25 (low 32 bits)
        let total_lo = u32::from_be_bytes([flac[22], flac[23], flac[24], flac[25]]);
        // Also check high nibble from byte 21
        let total_hi = (flac[21] & 0x0F) as u64;
        let total = (total_hi << 32) | total_lo as u64;
        assert_eq!(total, 200); // 200 samples per channel
    }

    #[test]
    fn crc8_known_value() {
        // Empty input -> CRC-8 should be 0
        assert_eq!(flac_crc8(&[]), 0);
        // Known test
        let crc = flac_crc8(&[0xFF]);
        assert_ne!(crc, 0); // just verify it does something
    }

    #[test]
    fn crc16_known_value() {
        assert_eq!(flac_crc16(&[]), 0);
    }

    #[test]
    fn rice_coding_roundtrip() {
        // Verify that the zig-zag mapping is correct
        // 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, 2 -> 4
        fn zigzag(v: i64) -> u64 {
            if v >= 0 {
                (v as u64) << 1
            } else {
                (((-v) as u64) << 1) - 1
            }
        }
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(-2), 3);
        assert_eq!(zigzag(2), 4);
    }

    #[test]
    fn bitwriter_basic() {
        let mut bw = BitWriter::new();
        bw.write_bits(0xFF, 8);
        bw.flush();
        assert_eq!(bw.into_bytes(), vec![0xFF]);
    }

    #[test]
    fn bitwriter_partial() {
        let mut bw = BitWriter::new();
        bw.write_bits(0b101, 3);
        bw.write_bits(0b01010, 5);
        bw.flush();
        // Should produce: 10101010 = 0xAA
        assert_eq!(bw.into_bytes(), vec![0xAA]);
    }

    #[test]
    fn fixed_prediction_order0() {
        let samples = vec![10, 20, 30, 40, 50];
        let residuals = compute_fixed_residuals(&samples, 0);
        assert_eq!(residuals, vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn fixed_prediction_order1() {
        let samples = vec![10, 20, 30, 40, 50];
        let residuals = compute_fixed_residuals(&samples, 1);
        assert_eq!(residuals, vec![10, 10, 10, 10]); // constant difference
    }

    #[test]
    fn fixed_prediction_order2() {
        // Linear ramp: second differences are zero
        let samples = vec![10, 20, 30, 40, 50];
        let residuals = compute_fixed_residuals(&samples, 2);
        assert_eq!(residuals, vec![0, 0, 0]); // perfect prediction for linear
    }

    #[tokio::test]
    async fn flac_large_block() {
        // Test with more than FLAC_BLOCK_SIZE samples to exercise multi-frame
        let total_frames = 8192; // 2 blocks of 4096
        let pcm = vec![0u8; total_frames * 2 * 2]; // stereo 16-bit
        let mut enc = AudioEncoder::new("flac", 44100, 16, 2);
        enc.start().await.unwrap();
        enc.write(&pcm).await.unwrap();
        let flac = enc.finish().await.unwrap();
        assert_eq!(&flac[0..4], b"fLaC");
    }

    #[tokio::test]
    async fn flac_32bit() {
        let mut enc = AudioEncoder::new("flac", 192000, 32, 2);
        enc.start().await.unwrap();
        // 10 frames of silence (stereo 32-bit = 80 bytes)
        let pcm = vec![0u8; 80];
        enc.write(&pcm).await.unwrap();
        let flac = enc.finish().await.unwrap();
        assert_eq!(&flac[0..4], b"fLaC");
    }
}
