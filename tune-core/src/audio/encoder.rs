use std::io::Cursor;

use tracing::{debug, warn};

/// Audio encoder that handles WAV (via hound) and FLAC (native pure-Rust).
/// MP3 and OGG requests are transparently encoded as FLAC with a warning,
/// since pure-Rust encoders for those formats are not available.
///
/// The FLAC encoder works in streaming mode: PCM data is encoded incrementally
/// as it arrives via `write()`, keeping memory usage bounded regardless of
/// track length. Only one block's worth of samples (~4096) is buffered at a time.
pub struct AudioEncoder {
    format: String,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    /// WAV-only: accumulates all PCM data (hound needs it all upfront).
    pcm_buffer: Option<Vec<u8>>,
    /// FLAC streaming state (None when not started or after finish).
    flac_state: Option<FlacStreamState>,
}

/// Mutable state for the streaming FLAC encoder.
struct FlacStreamState {
    /// Encoded output accumulated across start/write/finish.
    output: Vec<u8>,
    /// Per-channel sample buffers — holds up to FLAC_BLOCK_SIZE samples each.
    channel_buffers: Vec<Vec<i32>>,
    /// Number of full FLAC frames encoded so far.
    frame_count: u32,
    /// Total number of inter-channel samples encoded so far.
    total_samples: u64,
    /// Minimum encoded frame size seen so far (bytes).
    min_frame_size: u32,
    /// Maximum encoded frame size seen so far (bytes).
    max_frame_size: u32,
    /// Byte offset of the STREAMINFO block within `output`.
    streaminfo_offset: usize,
    /// Leftover PCM bytes from a previous `write()` that didn't align to a
    /// complete inter-channel sample boundary (< bytes_per_frame bytes).
    pcm_leftover: Vec<u8>,
}

impl AudioEncoder {
    pub fn new(format: &str, sample_rate: u32, bit_depth: u32, channels: u32) -> Self {
        Self {
            format: format.to_string(),
            sample_rate,
            bit_depth,
            channels,
            pcm_buffer: None,
            flac_state: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        self.start_sync()
    }

    /// Synchronous body of [`start`]. Encoding is pure CPU work (native FLAC /
    /// WAV, no I/O or await points), so it can run directly on a blocking
    /// thread without a Tokio runtime. Call this from inside `spawn_blocking`
    /// instead of driving the async method with a nested `Handle::block_on`,
    /// which can deadlock the runtime.
    pub fn start_sync(&mut self) -> Result<(), String> {
        match self.format.as_str() {
            "wav" => {
                debug!(
                    format = "wav",
                    sample_rate = self.sample_rate,
                    bit_depth = self.bit_depth,
                    "encoder_start_hound"
                );
                self.pcm_buffer = Some(Vec::new());
            }
            "flac" => {
                debug!(
                    format = "flac",
                    sample_rate = self.sample_rate,
                    bit_depth = self.bit_depth,
                    "encoder_start_native_flac_streaming"
                );
                self.flac_state = Some(flac_start(self.sample_rate, self.bit_depth, self.channels));
            }
            "mp3" => {
                warn!(
                    requested = "mp3",
                    actual = "flac",
                    "encoder_format_substitution: MP3 not natively supported, encoding as FLAC"
                );
                self.flac_state = Some(flac_start(self.sample_rate, self.bit_depth, self.channels));
            }
            "ogg" => {
                warn!(
                    requested = "ogg",
                    actual = "flac",
                    "encoder_format_substitution: OGG not natively supported, encoding as FLAC"
                );
                self.flac_state = Some(flac_start(self.sample_rate, self.bit_depth, self.channels));
            }
            other => {
                warn!(
                    requested = other,
                    actual = "flac",
                    "encoder_format_substitution: format not natively supported, encoding as FLAC"
                );
                self.flac_state = Some(flac_start(self.sample_rate, self.bit_depth, self.channels));
            }
        }
        Ok(())
    }

    pub async fn write(&mut self, pcm_data: &[u8]) -> Result<(), String> {
        self.write_sync(pcm_data)
    }

    /// Synchronous body of [`write`] — see [`start_sync`](Self::start_sync).
    pub fn write_sync(&mut self, pcm_data: &[u8]) -> Result<(), String> {
        if let Some(ref mut state) = self.flac_state {
            // FLAC streaming path: decode + buffer + encode when full
            flac_write(
                state,
                pcm_data,
                self.sample_rate,
                self.bit_depth,
                self.channels,
            )?;
            return Ok(());
        }
        // WAV path: accumulate raw PCM
        let buf = self.pcm_buffer.as_mut().ok_or("encoder not started")?;
        buf.extend_from_slice(pcm_data);
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        self.finish_sync()
    }

    /// Synchronous body of [`finish`] — see [`start_sync`](Self::start_sync).
    pub fn finish_sync(&mut self) -> Result<Vec<u8>, String> {
        // FLAC streaming path
        if let Some(state) = self.flac_state.take() {
            return flac_finish(state, self.sample_rate, self.bit_depth, self.channels);
        }

        // WAV path
        let pcm_data = self.pcm_buffer.take().unwrap_or_default();
        if pcm_data.is_empty() && self.pcm_buffer.is_none() {
            // finish() called without start()
            return Ok(Vec::new());
        }

        match self.format.as_str() {
            "wav" => encode_wav_hound(&pcm_data, self.sample_rate, self.bit_depth, self.channels),
            _ => {
                // Should not reach here (FLAC uses flac_state), but as a fallback:
                encode_flac_batch(&pcm_data, self.sample_rate, self.bit_depth, self.channels)
            }
        }
    }

    pub async fn stop(&mut self) {
        self.pcm_buffer = None;
        self.flac_state = None;
    }
}

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
// FLAC encoder (pure Rust, streaming)
// ---------------------------------------------------------------------------
//
// Implements a minimal FLAC encoder using FIXED prediction (orders 0-4)
// with Rice coding for residuals. Produces valid FLAC streams that any
// standard decoder can read.
//
// The encoder works in streaming mode:
//   start()  -> writes fLaC magic + placeholder STREAMINFO
//   write()  -> de-interleaves PCM, encodes full blocks immediately
//   finish() -> encodes remaining samples, patches STREAMINFO, returns output

const FLAC_BLOCK_SIZE: usize = 4096;

/// Initialize FLAC streaming state: write the fLaC magic and a placeholder
/// STREAMINFO block. Returns the mutable state to be stored in AudioEncoder.
fn flac_start(sample_rate: u32, bit_depth: u32, channels: u32) -> FlacStreamState {
    let mut output = Vec::with_capacity(8192);

    // 1. fLaC magic
    output.extend_from_slice(b"fLaC");

    // 2. STREAMINFO metadata block header
    //    1 bit is_last=0, 7 bits type=0, 24 bits length=34
    let block_header: u32 = (0 << 31) | 34; // is_last=0: VORBIS_COMMENT follows
    output.extend_from_slice(&block_header.to_be_bytes());

    let streaminfo_offset = output.len();

    // min block size, max block size (2+2 bytes) — placeholder using FLAC_BLOCK_SIZE
    let block_size_u16 = FLAC_BLOCK_SIZE as u16;
    output.extend_from_slice(&block_size_u16.to_be_bytes());
    output.extend_from_slice(&block_size_u16.to_be_bytes());

    // min frame size, max frame size (3+3 bytes) — placeholder zeros
    output.extend_from_slice(&[0u8; 6]);

    // sample rate (20 bits) | channels-1 (3 bits) | bps-1 (5 bits) | total samples high 4 bits
    // Total samples = 0 placeholder (patched in finish)
    let sr_ch_bps: u32 = ((sample_rate) << 12)
        | (((channels - 1) as u32) << 9)
        | (((bit_depth - 1) as u32) << 4)
        | 0; // total_samples_hi = 0 placeholder
    output.extend_from_slice(&sr_ch_bps.to_be_bytes());

    // total samples low 32 bits — placeholder 0
    output.extend_from_slice(&0u32.to_be_bytes());

    // MD5 — 16 bytes of zeros (optional, valid per spec)
    output.extend_from_slice(&[0u8; 16]);

    // 3. Empty VORBIS_COMMENT block (is_last=1, type=4, length=8)
    //    All reference encoders (flac, ffmpeg) emit this block.
    //    Some DLNA renderers reject FLAC without it.
    append_empty_vorbis_comment(&mut output);

    FlacStreamState {
        output,
        channel_buffers: vec![Vec::with_capacity(FLAC_BLOCK_SIZE); channels as usize],
        frame_count: 0,
        total_samples: 0,
        min_frame_size: u32::MAX,
        max_frame_size: 0,
        streaminfo_offset,
        pcm_leftover: Vec::new(),
    }
}

/// Feed PCM data into the streaming FLAC encoder. Encodes complete blocks
/// immediately, keeping only up to FLAC_BLOCK_SIZE samples buffered.
fn flac_write(
    state: &mut FlacStreamState,
    pcm_data: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<(), String> {
    let bytes_per_sample = ((bit_depth + 7) / 8) as usize;
    let bytes_per_frame = bytes_per_sample * channels as usize; // one inter-channel sample

    // Combine any leftover bytes from the previous write with new data.
    // We take ownership of leftover to avoid borrow conflicts with state.
    let combined: Vec<u8>;
    let working_data: &[u8] = if state.pcm_leftover.is_empty() {
        pcm_data
    } else {
        combined = {
            let mut v = std::mem::take(&mut state.pcm_leftover);
            v.extend_from_slice(pcm_data);
            v
        };
        &combined
    };

    // How many complete inter-channel samples can we decode?
    let usable_bytes = (working_data.len() / bytes_per_frame) * bytes_per_frame;
    let remainder_bytes = working_data.len() - usable_bytes;

    // Decode usable PCM to interleaved i32 samples
    let interleaved = pcm_to_i32(&working_data[..usable_bytes], bit_depth)?;

    // De-interleave into per-channel buffers
    let ch = channels as usize;
    for (i, &s) in interleaved.iter().enumerate() {
        state.channel_buffers[i % ch].push(s);
    }

    // Encode complete blocks as they become available
    while state.channel_buffers[0].len() >= FLAC_BLOCK_SIZE {
        encode_block_from_buffers(state, FLAC_BLOCK_SIZE, sample_rate, bit_depth, channels)?;
    }

    // Save remainder bytes for the next write
    if remainder_bytes > 0 {
        state.pcm_leftover = working_data[usable_bytes..].to_vec();
    }
    // else: pcm_leftover is already empty (either was empty, or was taken via mem::take)

    Ok(())
}

/// Encode `block_len` samples from the front of each channel buffer as a single
/// FLAC frame, then drain those samples from the buffers.
fn encode_block_from_buffers(
    state: &mut FlacStreamState,
    block_len: usize,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<(), String> {
    let frame_start = state.output.len();

    // Build temporary per-channel slices for the encoder
    let channel_slices: Vec<&[i32]> = state
        .channel_buffers
        .iter()
        .map(|buf| &buf[..block_len])
        .collect();

    encode_flac_frame_slices(
        &mut state.output,
        &channel_slices,
        block_len,
        sample_rate,
        bit_depth,
        channels,
        state.frame_count,
    )?;

    let frame_bytes = (state.output.len() - frame_start) as u32;
    state.min_frame_size = state.min_frame_size.min(frame_bytes);
    state.max_frame_size = state.max_frame_size.max(frame_bytes);
    state.frame_count += 1;
    state.total_samples += block_len as u64;

    // Drain the consumed samples from channel buffers
    for buf in &mut state.channel_buffers {
        buf.drain(..block_len);
    }

    Ok(())
}

/// Finalize the FLAC stream: encode remaining samples, patch STREAMINFO, return output.
fn flac_finish(
    mut state: FlacStreamState,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    // Handle any leftover PCM bytes (shouldn't happen with well-aligned writes, but be safe)
    if !state.pcm_leftover.is_empty() {
        let bytes_per_sample = ((bit_depth + 7) / 8) as usize;
        let bytes_per_frame = bytes_per_sample * channels as usize;
        let usable = (state.pcm_leftover.len() / bytes_per_frame) * bytes_per_frame;
        if usable > 0 {
            let interleaved = pcm_to_i32(&state.pcm_leftover[..usable], bit_depth)?;
            let ch = channels as usize;
            for (i, &s) in interleaved.iter().enumerate() {
                state.channel_buffers[i % ch].push(s);
            }
        }
        state.pcm_leftover.clear();
    }

    // Encode any remaining samples as a final (possibly shorter) frame
    let remaining = state.channel_buffers[0].len();
    if remaining > 0 {
        encode_block_from_buffers(&mut state, remaining, sample_rate, bit_depth, channels)?;
    }

    let total_samples = state.total_samples;

    // Patch STREAMINFO with actual values
    let si = state.streaminfo_offset;
    let output = &mut state.output;

    // Patch min/max block size (bytes 0-3 of STREAMINFO)
    // If only one frame was encoded and it was shorter than FLAC_BLOCK_SIZE,
    // use the actual block size
    if total_samples > 0 {
        let actual_max_block = FLAC_BLOCK_SIZE.min(total_samples as usize) as u16;
        // Per the FLAC spec, STREAMINFO min block size is the smallest block
        // *excluding the last block*. With a single frame, min = max = that
        // frame. With several frames, every non-last block is FLAC_BLOCK_SIZE,
        // so min = FLAC_BLOCK_SIZE even when the last frame is shorter.
        // Reporting the short last block here made min != max, which strict
        // decoders (symphonia, CoreAudio) read as a variable-block stream that
        // conflicts with the fixed blocking-strategy flag → whole stream
        // rejected ("unexpected end of file"), so the transcoded FLAC's final
        // frame broke playback.
        let actual_min_block = if total_samples <= FLAC_BLOCK_SIZE as u64 {
            total_samples as u16
        } else {
            FLAC_BLOCK_SIZE as u16
        };
        output[si..si + 2].copy_from_slice(&actual_min_block.to_be_bytes());
        output[si + 2..si + 4].copy_from_slice(&actual_max_block.to_be_bytes());
    } else {
        // No samples at all — set both to 0
        output[si..si + 4].copy_from_slice(&[0u8; 4]);
    }

    // Patch min/max frame size (bytes 4-9 of STREAMINFO)
    let mut min_fs = state.min_frame_size;
    if min_fs == u32::MAX {
        min_fs = 0;
    }
    let fs_offset = si + 4;
    output[fs_offset] = (min_fs >> 16) as u8;
    output[fs_offset + 1] = (min_fs >> 8) as u8;
    output[fs_offset + 2] = min_fs as u8;
    output[fs_offset + 3] = (state.max_frame_size >> 16) as u8;
    output[fs_offset + 4] = (state.max_frame_size >> 8) as u8;
    output[fs_offset + 5] = state.max_frame_size as u8;

    // Patch total samples: high 4 bits at byte 17 of STREAMINFO (nibble in sr_ch_bps word),
    // low 32 bits at bytes 18-21 of STREAMINFO.
    // sr_ch_bps is at si+10..si+14, total_lo at si+14..si+18
    let total_hi_nibble = ((total_samples >> 32) & 0xF) as u8;
    // Preserve existing sr/ch/bps bits, patch only the low nibble
    output[si + 13] = (output[si + 13] & 0xF0) | total_hi_nibble;
    output[si + 14..si + 18].copy_from_slice(&(total_samples as u32).to_be_bytes());

    debug!(
        flac_bytes = output.len(),
        total_samples,
        frames = state.frame_count,
        "flac_encoded_streaming"
    );

    Ok(state.output)
}

/// Batch FLAC encoding (used as internal fallback, keeps original logic).
fn encode_flac_batch(
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

    // 2. STREAMINFO metadata block (is_last=0: VORBIS_COMMENT follows)
    let block_header: u32 = (0 << 31) | 34;
    output.extend_from_slice(&block_header.to_be_bytes());

    let block_size = FLAC_BLOCK_SIZE.min(total_samples);
    let block_size_u16 = if block_size == 0 {
        1u16
    } else {
        block_size as u16
    };

    let streaminfo_offset = output.len();

    output.extend_from_slice(&block_size_u16.to_be_bytes());
    output.extend_from_slice(&block_size_u16.to_be_bytes());
    output.extend_from_slice(&[0u8; 6]);
    let sr_ch_bps: u64 = ((sample_rate as u64) << 12)
        | (((channels - 1) as u64) << 9)
        | (((bit_depth - 1) as u64) << 4)
        | ((total_samples as u64 >> 32) & 0xF);
    output.extend_from_slice(&(sr_ch_bps as u32).to_be_bytes());
    output.extend_from_slice(&(total_samples as u32).to_be_bytes());
    output.extend_from_slice(&[0u8; 16]);

    // 3. Empty VORBIS_COMMENT block
    append_empty_vorbis_comment(&mut output);

    // 3. Audio frames
    let mut min_frame_size: u32 = u32::MAX;
    let mut max_frame_size: u32 = 0;
    let mut sample_offset: usize = 0;
    let mut frame_number: u32 = 0;

    while sample_offset < total_samples {
        let block_len = FLAC_BLOCK_SIZE.min(total_samples - sample_offset);
        let frame_start = output.len();

        let channel_slices: Vec<&[i32]> = channel_data
            .iter()
            .map(|ch_buf| &ch_buf[sample_offset..sample_offset + block_len])
            .collect();

        encode_flac_frame_slices(
            &mut output,
            &channel_slices,
            block_len,
            sample_rate,
            bit_depth,
            channels,
            frame_number,
        )?;

        let frame_bytes = (output.len() - frame_start) as u32;
        min_frame_size = min_frame_size.min(frame_bytes);
        max_frame_size = max_frame_size.max(frame_bytes);

        sample_offset += block_len;
        frame_number += 1;
    }

    // Patch min/max frame size in STREAMINFO
    if min_frame_size == u32::MAX {
        min_frame_size = 0;
    }
    let fs_offset = streaminfo_offset + 4;
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

/// Append an empty VORBIS_COMMENT metadata block (is_last=1, type=4).
/// Body: vendor_length(0, LE) + user_comment_count(0, LE) = 8 bytes.
fn append_empty_vorbis_comment(output: &mut Vec<u8>) {
    let vc_header: u32 = (1 << 31) | (4 << 24) | 8; // is_last=1, type=4, length=8
    output.extend_from_slice(&vc_header.to_be_bytes());
    output.extend_from_slice(&0u32.to_le_bytes()); // vendor_length = 0
    output.extend_from_slice(&0u32.to_le_bytes()); // user_comment_count = 0
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

/// Encode a single FLAC frame from per-channel slices and append to output.
/// `channel_slices[ch]` contains exactly `block_len` samples for channel `ch`.
fn encode_flac_frame_slices(
    output: &mut Vec<u8>,
    channel_slices: &[&[i32]],
    block_len: usize,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    frame_number: u32,
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
        32 => 7,
        _ => 0,
    };
    bw.write_bits(sample_size_code as u32, 3);

    // Reserved: 1 bit = 0
    bw.write_bits(0, 1);

    // Frame number (UTF-8 coded, blocking strategy=0 means frame number)
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
        let samples = channel_slices[ch_idx];
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

    // Unary code for quotient: `quotient` zeros followed by a terminating one.
    // FLAC decoders read the quotient with read_unary_zeros() (count leading
    // zeros up to the first 1), so writing ones-then-zero desynchronised every
    // Rice-coded residual — corrupting all FIXED subframes (audible as noise on
    // transcoded ALAC; CONSTANT subframes, which use no Rice coding, were fine).
    for _ in 0..quotient {
        bw.write_bits(0, 1);
    }
    bw.write_bits(1, 1);

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

    /// Encode a known 24-bit signal to FLAC and decode it back: the stream must
    /// be valid and bit-exact. Guards two encoder bugs that made transcoded
    /// ALAC play as noise: (1) the Rice quotient was written as ones-then-zero
    /// instead of the FLAC zeros-then-one convention, corrupting every FIXED
    /// subframe; (2) STREAMINFO min_block_size reported the short final block,
    /// making min != max so strict decoders rejected the whole stream. The
    /// length (5000) is deliberately not a multiple of the 4096 block size so a
    /// partial final frame is exercised.
    #[test]
    fn flac_24bit_roundtrip_is_bit_exact() {
        let n = 5000usize;
        let amp = 7_549_746i32; // 0.9 * (2^23 - 1)
        let mono: Vec<i32> = (0..n)
            .map(|i| {
                (amp as f64 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48000.0).sin())
                    as i32
            })
            .collect();

        // Interleave to stereo, pack as 24-bit little-endian PCM.
        let mut pcm = Vec::with_capacity(n * 2 * 3);
        for &s in &mono {
            let b = s.to_le_bytes();
            for _ in 0..2 {
                pcm.extend_from_slice(&b[..3]);
            }
        }

        let mut enc = AudioEncoder::new("flac", 48000, 24, 2);
        enc.start_sync().expect("start");
        enc.write_sync(&pcm).expect("write");
        let flac = enc.finish_sync().expect("finish");
        assert!(flac.len() > 64, "flac output too small");

        let tmp = std::env::temp_dir().join(format!("tune-flac-rt-{n}.flac"));
        std::fs::write(&tmp, &flac).expect("write temp flac");
        let decoded =
            crate::audio::decode::decode_to_pcm(tmp.to_str().unwrap(), None, None, 0.0, 0.0)
                .expect("decoding our own FLAC must succeed");
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(decoded.bit_depth, 24, "bit depth preserved");
        assert_eq!(decoded.sample_rate, 48000, "sample rate preserved");
        assert_eq!(decoded.channels, 2, "channels preserved");
        assert_eq!(decoded.samples_i32.len(), n * 2, "sample count preserved");

        // Every decoded sample must equal the input (FLAC is lossless).
        let expected: Vec<i32> = mono.iter().flat_map(|&s| [s, s]).collect();
        for (i, (&e, &g)) in expected.iter().zip(decoded.samples_i32.iter()).enumerate() {
            assert_eq!(e, g, "sample {i} mismatch: {e} != {g}");
        }
    }

    #[test]
    fn encoder_new() {
        let enc = AudioEncoder::new("flac", 44100, 16, 2);
        assert_eq!(enc.format, "flac");
        assert_eq!(enc.sample_rate, 44100);
        assert!(enc.pcm_buffer.is_none());
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

        // Check STREAMINFO block header (is_last=0, type=0, length=34)
        let block_hdr = u32::from_be_bytes([flac[4], flac[5], flac[6], flac[7]]);
        assert_eq!(block_hdr & (0x7F << 24), 0); // type = 0 (STREAMINFO)
        assert_eq!(block_hdr & 0xFFFFFF, 34); // length = 34
        assert_eq!(block_hdr >> 31, 0); // is_last = 0 (VORBIS_COMMENT follows)

        // Check VORBIS_COMMENT block at offset 42 (4 magic + 4 header + 34 streaminfo)
        let vc_hdr = u32::from_be_bytes([flac[42], flac[43], flac[44], flac[45]]);
        assert_eq!(vc_hdr >> 31, 1); // is_last = 1
        assert_eq!((vc_hdr >> 24) & 0x7F, 4); // type = 4 (VORBIS_COMMENT)
        assert_eq!(vc_hdr & 0xFFFFFF, 8); // length = 8
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
        // Use the batch encoder directly to test STREAMINFO encoding
        let pcm = vec![0u8; 800]; // 200 frames stereo 16-bit
        let flac = encode_flac_batch(&pcm, 44100, 16, 2).unwrap();
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

    /// Verify that streaming encoding (multiple small writes) produces
    /// the same FLAC output structure as a single large write.
    #[tokio::test]
    async fn streaming_matches_batch_encoding() {
        // Generate a non-trivial signal: stereo 16-bit ramp, 5000 samples
        // (exceeds one FLAC_BLOCK_SIZE=4096 to test multi-frame)
        let num_samples = 5000usize;
        let channels = 2u32;
        let bit_depth = 16u32;
        let sample_rate = 44100u32;
        let bytes_per_sample = 2usize;

        let mut pcm = Vec::with_capacity(num_samples * channels as usize * bytes_per_sample);
        for i in 0..num_samples {
            for ch in 0..channels {
                let val = (((i * channels as usize + ch as usize) % 512) as i16 - 256) * 50;
                pcm.extend_from_slice(&val.to_le_bytes());
            }
        }

        // Batch encoding (single write)
        let mut enc_batch = AudioEncoder::new("flac", sample_rate, bit_depth, channels);
        enc_batch.start().await.unwrap();
        enc_batch.write(&pcm).await.unwrap();
        let flac_batch = enc_batch.finish().await.unwrap();

        // Streaming encoding (many small writes of ~100 samples each)
        let mut enc_stream = AudioEncoder::new("flac", sample_rate, bit_depth, channels);
        enc_stream.start().await.unwrap();
        let chunk_size = 100 * channels as usize * bytes_per_sample; // 100 samples per write
        for chunk in pcm.chunks(chunk_size) {
            enc_stream.write(chunk).await.unwrap();
        }
        let flac_stream = enc_stream.finish().await.unwrap();

        // Both should produce valid FLAC
        assert_eq!(&flac_batch[0..4], b"fLaC");
        assert_eq!(&flac_stream[0..4], b"fLaC");

        // The output should be byte-identical since the encoding algorithm
        // is deterministic and processes the same blocks in the same order.
        assert_eq!(
            flac_batch.len(),
            flac_stream.len(),
            "batch and streaming FLAC should have the same length"
        );
        assert_eq!(
            flac_batch, flac_stream,
            "batch and streaming FLAC should be byte-identical"
        );
    }

    /// Verify that streaming encoding with many blocks keeps memory bounded.
    /// The encoder should not accumulate all PCM data — only one block at a time.
    #[tokio::test]
    async fn streaming_bounded_memory_large_input() {
        let channels = 2u32;
        let bit_depth = 16u32;
        let sample_rate = 44100u32;
        let bytes_per_sample = 2usize;
        let frame_bytes = bytes_per_sample * channels as usize;

        // Simulate a long track: 10 * FLAC_BLOCK_SIZE samples = ~40960 samples
        // This is enough to verify multi-block streaming works without OOM.
        let total_samples = FLAC_BLOCK_SIZE * 10;
        let total_pcm_bytes = total_samples * frame_bytes;

        let mut enc = AudioEncoder::new("flac", sample_rate, bit_depth, channels);
        enc.start().await.unwrap();

        // Write in small chunks (256 samples at a time)
        let chunk_samples = 256;
        let chunk_bytes = chunk_samples * frame_bytes;
        let mut pcm_chunk = vec![0u8; chunk_bytes];
        let mut written = 0usize;
        while written < total_pcm_bytes {
            let this_chunk = chunk_bytes.min(total_pcm_bytes - written);
            // Fill with a pattern so it's not all zeros (exercises prediction)
            for j in 0..this_chunk {
                pcm_chunk[j] = ((written + j) % 251) as u8;
            }
            enc.write(&pcm_chunk[..this_chunk]).await.unwrap();

            // Verify the channel buffers never exceed FLAC_BLOCK_SIZE
            if let Some(ref state) = enc.flac_state {
                for buf in &state.channel_buffers {
                    assert!(
                        buf.len() <= FLAC_BLOCK_SIZE,
                        "channel buffer grew to {} samples, exceeding FLAC_BLOCK_SIZE={}",
                        buf.len(),
                        FLAC_BLOCK_SIZE,
                    );
                }
            }

            written += this_chunk;
        }

        let flac = enc.finish().await.unwrap();
        assert_eq!(&flac[0..4], b"fLaC");

        // Verify total samples in STREAMINFO
        let total_lo = u32::from_be_bytes([flac[22], flac[23], flac[24], flac[25]]);
        let total_hi = (flac[21] & 0x0F) as u64;
        let encoded_total = (total_hi << 32) | total_lo as u64;
        assert_eq!(
            encoded_total, total_samples as u64,
            "STREAMINFO total_samples should match"
        );

        // Verify min/max frame sizes were patched (non-zero)
        let min_fs = u32::from_be_bytes([0, flac[12], flac[13], flac[14]]);
        let max_fs = u32::from_be_bytes([0, flac[15], flac[16], flac[17]]);
        assert!(min_fs > 0, "min_frame_size should be patched");
        assert!(max_fs > 0, "max_frame_size should be patched");
        assert!(max_fs >= min_fs, "max_frame_size >= min_frame_size");
    }

    /// Verify streaming works with unaligned write boundaries (PCM chunks that
    /// don't align to sample boundaries).
    #[tokio::test]
    async fn streaming_unaligned_writes() {
        let channels = 2u32;
        let bit_depth = 16u32;
        let sample_rate = 44100u32;

        // 500 samples, stereo 16-bit = 2000 bytes
        let mut pcm = Vec::with_capacity(2000);
        for i in 0..500 {
            for ch in 0..2 {
                let val = ((i * 2 + ch) as i16).wrapping_mul(73);
                pcm.extend_from_slice(&val.to_le_bytes());
            }
        }

        // Write in chunks of 137 bytes (not a multiple of 4 = frame size)
        let mut enc = AudioEncoder::new("flac", sample_rate, bit_depth, channels);
        enc.start().await.unwrap();
        for chunk in pcm.chunks(137) {
            enc.write(chunk).await.unwrap();
        }
        let flac = enc.finish().await.unwrap();

        assert_eq!(&flac[0..4], b"fLaC");

        // Cross-check: single write should produce the same output
        let mut enc2 = AudioEncoder::new("flac", sample_rate, bit_depth, channels);
        enc2.start().await.unwrap();
        enc2.write(&pcm).await.unwrap();
        let flac2 = enc2.finish().await.unwrap();

        assert_eq!(
            flac, flac2,
            "unaligned writes should produce same output as single write"
        );
    }
}
