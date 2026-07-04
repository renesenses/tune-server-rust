//! DSD-to-PCM converter using FIR decimation.
//!
//! DSD is a 1-bit stream at very high sample rates (e.g., 2.8224 MHz for DSD64).
//! This module decimates DSD data to multi-bit PCM at a conventional sample rate
//! (e.g., 176.4 kHz or 88.2 kHz) using a windowed-sinc FIR lowpass filter.
//!
//! The converter produces 24-bit signed PCM samples packed as little-endian i32
//! values in the output byte stream (3 bytes per sample, packed as 4 bytes for
//! alignment, or as raw 24-bit LE).

use std::f64::consts::PI;

/// DSD-to-PCM decimation converter.
pub struct DsdToPcmConverter {
    /// How many DSD bits map to one PCM sample.
    decimation_ratio: usize,
    /// FIR filter coefficients (one per DSD sample in a decimation window).
    filter_coeffs: Vec<f64>,
    /// Number of audio channels.
    channels: usize,
    /// Output PCM sample rate in Hz.
    pub output_rate: u32,
    /// Output bit depth (always 24).
    pub output_depth: u32,
    /// Whether input DSD bits are LSB-first (DSF) or MSB-first (DFF).
    lsb_first: bool,
}

impl DsdToPcmConverter {
    /// Create a new converter.
    ///
    /// - `dsd_rate`: DSD sample rate (e.g., 2_822_400 for DSD64)
    /// - `target_rate`: desired PCM output rate (e.g., 176_400)
    /// - `channels`: number of audio channels
    /// - `lsb_first`: true for DSF (LSB first), false for DFF (MSB first)
    pub fn new(dsd_rate: u32, target_rate: u32, channels: usize, lsb_first: bool) -> Self {
        let decimation_ratio = (dsd_rate / target_rate) as usize;
        assert!(decimation_ratio > 0, "decimation ratio must be > 0");

        // FIR filter length: longer for higher decimation ratios
        let filter_len = match decimation_ratio {
            1..=16 => 256,
            17..=32 => 512,
            33..=64 => 1024,
            _ => 2048,
        };

        let filter_coeffs = design_lowpass_fir(filter_len, decimation_ratio);

        DsdToPcmConverter {
            decimation_ratio,
            filter_coeffs,
            channels,
            output_rate: target_rate,
            output_depth: 24,
            lsb_first,
        }
    }

    /// Convert DSD data bytes to PCM samples.
    ///
    /// Input: interleaved DSD bytes (ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...)
    /// Each byte contains 8 DSD samples.
    ///
    /// Output: interleaved 24-bit PCM samples as little-endian bytes (3 bytes per sample).
    /// Layout: ch0_sample0 (3 bytes), ch1_sample0 (3 bytes), ch0_sample1 (3 bytes), ...
    pub fn process(&self, dsd_data: &[u8]) -> Vec<u8> {
        let channels = self.channels;
        if channels == 0 || dsd_data.is_empty() {
            return Vec::new();
        }

        // Total DSD bytes per channel
        let total_bytes = dsd_data.len() / channels;
        // Total DSD samples (bits) per channel
        let total_dsd_samples = total_bytes * 8;
        // Number of output PCM samples per channel
        let output_samples_per_ch = total_dsd_samples / self.decimation_ratio;

        if output_samples_per_ch == 0 {
            return Vec::new();
        }

        // De-interleave DSD data into per-channel bit streams
        // and expand bytes to +1/-1 sample values for filtering
        let mut channel_bits: Vec<Vec<f64>> = vec![Vec::with_capacity(total_dsd_samples); channels];

        for byte_idx in 0..total_bytes {
            for ch in 0..channels {
                let src_idx = byte_idx * channels + ch;
                if src_idx >= dsd_data.len() {
                    break;
                }
                let byte = dsd_data[src_idx];
                // Extract 8 bits from this byte
                for bit in 0..8u8 {
                    let bit_val = if self.lsb_first {
                        // DSF: LSB first
                        (byte >> bit) & 1
                    } else {
                        // DFF: MSB first
                        (byte >> (7 - bit)) & 1
                    };
                    // DSD: 1 = positive, 0 = negative
                    channel_bits[ch].push(if bit_val == 1 { 1.0 } else { -1.0 });
                }
            }
        }

        // Apply FIR decimation filter per channel
        let filter_len = self.filter_coeffs.len();
        let half_filter = filter_len / 2;

        // Output buffer: 3 bytes per sample, interleaved channels
        let mut output = Vec::with_capacity(output_samples_per_ch * channels * 3);

        for sample_idx in 0..output_samples_per_ch {
            for ch in 0..channels {
                let center = sample_idx * self.decimation_ratio + self.decimation_ratio / 2;
                let mut sum = 0.0f64;

                for (k, &coeff) in self.filter_coeffs.iter().enumerate() {
                    // Position of this filter tap in the DSD stream
                    let pos = (center as isize) - (half_filter as isize) + (k as isize);
                    if pos >= 0 && (pos as usize) < channel_bits[ch].len() {
                        sum += channel_bits[ch][pos as usize] * coeff;
                    }
                }

                // Clamp to [-1.0, 1.0] and convert to 24-bit signed integer
                let clamped = sum.clamp(-1.0, 1.0);
                let pcm_val = (clamped * 8_388_607.0) as i32; // 2^23 - 1

                // Write as 24-bit little-endian
                let bytes = pcm_val.to_le_bytes();
                output.push(bytes[0]);
                output.push(bytes[1]);
                output.push(bytes[2]);
            }
        }

        output
    }

    /// Convert DSD data and return as i16 samples (for compatibility with DecodedAudio).
    ///
    /// This is a convenience wrapper that converts 24-bit output to 16-bit.
    pub fn process_to_i16(&self, dsd_data: &[u8]) -> Vec<i16> {
        let pcm_24 = self.process(dsd_data);
        let num_samples = pcm_24.len() / 3;
        let mut samples = Vec::with_capacity(num_samples);

        for i in 0..num_samples {
            let offset = i * 3;
            if offset + 2 >= pcm_24.len() {
                break;
            }
            // Reconstruct 24-bit signed value from LE bytes
            let lo = pcm_24[offset] as u32;
            let mid = pcm_24[offset + 1] as u32;
            let hi = pcm_24[offset + 2] as u32;
            let val24 = lo | (mid << 8) | (hi << 16);
            // Sign-extend from 24-bit to 32-bit
            let val32 = if val24 & 0x80_0000 != 0 {
                (val24 | 0xFF00_0000) as i32
            } else {
                val24 as i32
            };
            // Truncate 24-bit to 16-bit (shift right by 8)
            let val16 = (val32 >> 8) as i16;
            samples.push(val16);
        }

        samples
    }
}

/// Design a lowpass FIR filter using windowed sinc method.
///
/// - `length`: number of filter taps
/// - `decimation`: decimation ratio (cutoff = 0.45 / decimation)
fn design_lowpass_fir(length: usize, decimation: usize) -> Vec<f64> {
    let mut coeffs = vec![0.0f64; length];
    let cutoff = 0.45 / decimation as f64; // normalized cutoff frequency
    let center = (length - 1) as f64 / 2.0;

    for i in 0..length {
        let x = i as f64 - center;

        // Sinc function
        let sinc = if x.abs() < 1e-10 {
            2.0 * PI * cutoff
        } else {
            (2.0 * PI * cutoff * x).sin() / x
        };

        // Blackman-Harris window
        let n = i as f64 / (length - 1) as f64;
        let window = 0.35875 - 0.48829 * (2.0 * PI * n).cos() + 0.14128 * (4.0 * PI * n).cos()
            - 0.01168 * (6.0 * PI * n).cos();

        coeffs[i] = sinc * window;
    }

    // Normalize so that the filter has unity gain at DC
    let sum: f64 = coeffs.iter().sum();
    if sum.abs() > 1e-10 {
        for c in &mut coeffs {
            *c /= sum;
        }
    }

    coeffs
}

/// Choose the best output sample rate for a given DSD rate.
///
/// Returns a rate that's a clean integer divisor of the DSD rate,
/// preferring 176.4 kHz for DSD64 and 352.8 kHz for DSD128+.
pub fn choose_output_rate(dsd_rate: u32) -> u32 {
    match dsd_rate {
        r if r >= 11_000_000 => 352_800, // DSD256/512
        r if r >= 5_000_000 => 352_800,  // DSD128
        r if r >= 2_000_000 => 176_400,  // DSD64
        _ => 176_400,                    // fallback
    }
}

/// Streaming DSD-to-PCM converter that processes DSD data in chunks.
///
/// Unlike `DsdToPcmConverter::process()` which loads the entire DSD file
/// into memory (causing OOM for large DSD files -- a 5-min DSD64 stereo
/// file expands to ~13 GB of f64 arrays), this converter maintains a small
/// sliding buffer of just `filter_len` DSD samples per channel and produces
/// PCM output incrementally.
///
/// Memory usage: O(filter_len * channels) ≈ 16 KB regardless of file size.
pub struct DsdToPcmStreamer {
    /// How many DSD bits map to one PCM sample.
    decimation_ratio: usize,
    /// FIR filter coefficients.
    filter_coeffs: Vec<f64>,
    /// Number of audio channels.
    channels: usize,
    /// Output PCM sample rate in Hz.
    pub output_rate: u32,
    /// Output bit depth (always 24).
    pub output_depth: u32,
    /// Whether input DSD bits are LSB-first (DSF) or MSB-first (DFF).
    lsb_first: bool,
    /// Per-channel ring buffer of DSD sample values (+1.0 / -1.0).
    /// Length = filter_len per channel. We only keep as many samples
    /// as needed for the FIR filter window.
    ring_bufs: Vec<Vec<f64>>,
    /// Write position in the ring buffer (wraps at filter_len).
    ring_pos: usize,
    /// Total DSD samples fed so far (across all calls to `feed`), per channel.
    total_dsd_samples: usize,
    /// Number of PCM output samples already emitted per channel.
    output_sample_idx: usize,
}

impl DsdToPcmStreamer {
    /// Create a new streaming converter.
    ///
    /// - `dsd_rate`: DSD sample rate (e.g., 2_822_400 for DSD64)
    /// - `target_rate`: desired PCM output rate (e.g., 176_400)
    /// - `channels`: number of audio channels
    /// - `lsb_first`: true for DSF (LSB first), false for DFF (MSB first)
    pub fn new(dsd_rate: u32, target_rate: u32, channels: usize, lsb_first: bool) -> Self {
        let decimation_ratio = (dsd_rate / target_rate) as usize;
        assert!(decimation_ratio > 0, "decimation ratio must be > 0");

        let filter_len = match decimation_ratio {
            1..=16 => 256,
            17..=32 => 512,
            33..=64 => 1024,
            _ => 2048,
        };

        let filter_coeffs = design_lowpass_fir(filter_len, decimation_ratio);

        DsdToPcmStreamer {
            decimation_ratio,
            filter_coeffs: filter_coeffs.clone(),
            channels,
            output_rate: target_rate,
            output_depth: 24,
            lsb_first,
            ring_bufs: vec![vec![0.0f64; filter_len]; channels],
            ring_pos: 0,
            total_dsd_samples: 0,
            output_sample_idx: 0,
        }
    }

    /// Feed a chunk of interleaved DSD bytes and return the resulting PCM.
    ///
    /// Input layout: ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...
    /// (same interleaving as DSF after de-blocking, or DFF natively).
    ///
    /// Output: 24-bit LE PCM bytes (3 bytes per sample, interleaved channels).
    ///
    /// Call this repeatedly with successive chunks from the file. The converter
    /// maintains internal state between calls. The chunk size can vary.
    pub fn feed(&mut self, dsd_chunk: &[u8]) -> Vec<u8> {
        let channels = self.channels;
        if channels == 0 || dsd_chunk.is_empty() {
            return Vec::new();
        }

        let filter_len = self.filter_coeffs.len();
        let half_filter = filter_len / 2;

        // Parse input bytes into DSD sample values and push into ring buffers
        let total_bytes = dsd_chunk.len() / channels;

        // Estimate max output samples from this chunk
        let new_dsd_samples = total_bytes * 8;
        let total_after = self.total_dsd_samples + new_dsd_samples;
        let max_new_output = total_after / self.decimation_ratio;
        let new_outputs = max_new_output.saturating_sub(self.output_sample_idx);
        let mut output = Vec::with_capacity(new_outputs * channels * 3);

        for byte_idx in 0..total_bytes {
            // Read the DSD byte for each channel at this byte position.
            // Each byte contains 8 DSD samples (1-bit each).
            // We must process one DSD sample at a time across ALL channels
            // (bit-interleaved) so that ring_pos advances uniformly.
            // The old code iterated `for ch { for bit }` which caused
            // all 8 bits of channel 0 to overwrite the same ring position
            // before ring_pos advanced — destroying 7/8 of channel 0's data.
            let mut ch_bytes = [0u8; 8]; // max 8 channels
            let mut valid_channels = channels;
            for ch in 0..channels {
                let src_idx = byte_idx * channels + ch;
                if src_idx >= dsd_chunk.len() {
                    valid_channels = ch;
                    break;
                }
                ch_bytes[ch] = dsd_chunk[src_idx];
            }
            if valid_channels == 0 {
                break;
            }

            for bit in 0..8u8 {
                // Write one DSD sample from each channel into its ring buffer
                for ch in 0..valid_channels {
                    let bit_val = if self.lsb_first {
                        (ch_bytes[ch] >> bit) & 1
                    } else {
                        (ch_bytes[ch] >> (7 - bit)) & 1
                    };
                    let sample = if bit_val == 1 { 1.0 } else { -1.0 };
                    self.ring_bufs[ch][self.ring_pos % filter_len] = sample;
                }

                // Advance ring_pos once per DSD sample position (all channels written)
                self.ring_pos += 1;
                self.total_dsd_samples += 1;

                // Check if we can emit a new PCM sample.
                // An output sample at index `n` is centered at DSD position:
                //   center = n * decimation_ratio + decimation_ratio / 2
                // We need all filter taps to be available, i.e. we need
                //   center + half_filter <= total_dsd_samples
                let next_center =
                    self.output_sample_idx * self.decimation_ratio + self.decimation_ratio / 2;
                let needed = next_center + half_filter;

                while needed <= self.total_dsd_samples && self.total_dsd_samples >= filter_len {
                    // Emit one PCM sample per channel
                    let center =
                        self.output_sample_idx * self.decimation_ratio + self.decimation_ratio / 2;

                    // Steady-state fast path: every emitted sample satisfies
                    // `center + half_filter == total_dsd_samples` (the emit loop
                    // fires as soon as `needed <= total`, and total increments by
                    // one, so equality is exact).  With filter_len == 2*half_filter
                    // the FIR window is then *exactly* the ring buffer contents
                    // [total-filter_len, total-1]: all taps are valid and map to a
                    // contiguous walk of the ring starting at the oldest sample
                    // (index ring_pos % filter_len).  That lets us drop the per-tap
                    // modulo + two bounds branches (the dominant cost) via a
                    // two-slice dot product.  Bit-identical to the general path
                    // below (same f64 values, same summation order k = 0..L-1).
                    // Only the handful of warm-up samples at stream start are
                    // misaligned (center + half_filter < total) and take the
                    // general path.
                    let aligned = center + half_filter == self.total_dsd_samples;
                    let ring_start = self.ring_pos % filter_len;

                    for emit_ch in 0..channels {
                        let sum = if aligned {
                            let ring = &self.ring_bufs[emit_ch];
                            let coeffs = &self.filter_coeffs;
                            let (head, tail) = ring.split_at(ring_start);
                            let split = tail.len(); // == filter_len - ring_start
                            let mut s = 0.0f64;
                            for (x, c) in tail.iter().zip(&coeffs[..split]) {
                                s += *x * *c;
                            }
                            for (x, c) in head.iter().zip(&coeffs[split..]) {
                                s += *x * *c;
                            }
                            s
                        } else {
                            // General path (warm-up / partial window). Absolute
                            // position `p` maps to ring index p % filter_len; taps
                            // outside the ring window are zero-padded.
                            let mut s = 0.0f64;
                            for (k, &coeff) in self.filter_coeffs.iter().enumerate() {
                                let pos = (center as isize) - (half_filter as isize) + (k as isize);
                                if pos >= 0 && (pos as usize) < self.total_dsd_samples {
                                    let age = self.total_dsd_samples - pos as usize;
                                    if age <= filter_len {
                                        let ring_idx =
                                            (self.ring_pos + filter_len - age) % filter_len;
                                        s += self.ring_bufs[emit_ch][ring_idx] * coeff;
                                    }
                                }
                            }
                            s
                        };

                        let clamped = sum.clamp(-1.0, 1.0);
                        let pcm_val = (clamped * 8_388_607.0) as i32;
                        let bytes = pcm_val.to_le_bytes();
                        output.push(bytes[0]);
                        output.push(bytes[1]);
                        output.push(bytes[2]);
                    }

                    self.output_sample_idx += 1;

                    // Re-check for next output sample
                    let next_center =
                        self.output_sample_idx * self.decimation_ratio + self.decimation_ratio / 2;
                    if next_center + half_filter > self.total_dsd_samples {
                        break;
                    }
                }
            }
        }

        output
    }

    /// Flush any remaining samples at the end of the stream.
    /// Produces PCM for any DSD samples that haven't been output yet
    /// (due to the FIR filter needing future samples that don't exist).
    pub fn flush(&mut self) -> Vec<u8> {
        let channels = self.channels;
        let filter_len = self.filter_coeffs.len();
        let half_filter = filter_len / 2;

        // How many more output samples could we theoretically produce?
        // The maximum output index is total_dsd_samples / decimation_ratio
        let max_outputs = if self.total_dsd_samples > 0 {
            self.total_dsd_samples / self.decimation_ratio
        } else {
            0
        };

        let remaining = max_outputs.saturating_sub(self.output_sample_idx);
        if remaining == 0 {
            return Vec::new();
        }

        let mut output = Vec::with_capacity(remaining * channels * 3);

        for _ in 0..remaining {
            let center = self.output_sample_idx * self.decimation_ratio + self.decimation_ratio / 2;

            for ch in 0..channels {
                let mut sum = 0.0f64;
                for (k, &coeff) in self.filter_coeffs.iter().enumerate() {
                    let pos = (center as isize) - (half_filter as isize) + (k as isize);
                    if pos >= 0 && (pos as usize) < self.total_dsd_samples {
                        let age = self.total_dsd_samples - pos as usize;
                        if age <= filter_len {
                            let ring_idx = (self.ring_pos + filter_len - age) % filter_len;
                            sum += self.ring_bufs[ch][ring_idx] * coeff;
                        }
                    }
                }

                let clamped = sum.clamp(-1.0, 1.0);
                let pcm_val = (clamped * 8_388_607.0) as i32;
                let bytes = pcm_val.to_le_bytes();
                output.push(bytes[0]);
                output.push(bytes[1]);
                output.push(bytes[2]);
            }

            self.output_sample_idx += 1;
        }

        output
    }

    /// Total number of PCM samples emitted so far (across all channels).
    pub fn total_output_samples(&self) -> usize {
        self.output_sample_idx * self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_coefficients_sum_to_one() {
        let coeffs = design_lowpass_fir(256, 16);
        let sum: f64 = coeffs.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "filter should have unity DC gain, got {sum}"
        );
    }

    #[test]
    fn filter_is_symmetric() {
        let coeffs = design_lowpass_fir(256, 16);
        let len = coeffs.len();
        for i in 0..len / 2 {
            assert!(
                (coeffs[i] - coeffs[len - 1 - i]).abs() < 1e-12,
                "filter should be symmetric at index {i}"
            );
        }
    }

    #[test]
    fn silence_dsd_produces_silence_pcm() {
        // All-zero DSD = constant negative = DC offset, but with a proper filter
        // it should produce a constant (near-DC) output.
        // All 0x00 means all bits are 0 => all -1.0 => strong negative DC.
        // All 0xFF means all bits are 1 => all +1.0 => strong positive DC.
        // A 50/50 mix (0x55 or 0xAA) approximates silence.

        // For true silence test: alternating 0x55 pattern (01010101 in binary)
        // With LSB-first (DSF), this gives alternating -1, +1, -1, +1...
        // which should produce near-zero PCM output.
        let channels = 2;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        // Generate enough data for at least a few output samples
        // Need decimation_ratio * output_samples DSD samples per channel
        // Each byte = 8 DSD samples, decimation = 16
        // So 1 output sample needs 16/8 = 2 bytes per channel
        let output_samples = 100;
        let bytes_per_ch = (output_samples * converter.decimation_ratio) / 8 + 128;
        let total_bytes = bytes_per_ch * channels;

        let dsd_data: Vec<u8> = (0..total_bytes).map(|_| 0x55u8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty(), "should produce PCM output");

        // All samples should be near zero (alternating DSD = ~silence)
        // Allow some filter ringing near edges
        let mid_start = pcm.len() / 4;
        let mid_end = 3 * pcm.len() / 4;
        for &s in &pcm[mid_start..mid_end] {
            assert!(
                s.abs() < 1000,
                "alternating DSD pattern should be near-silence, got {s}"
            );
        }
    }

    #[test]
    fn all_zeros_dsd_produces_negative_dc() {
        // All 0x00 = all bits 0 = all -1.0 => should produce large negative PCM values
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let bytes_per_ch = 1024;
        let dsd_data = vec![0x00u8; bytes_per_ch];
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        // Middle samples should be strongly negative
        let mid = pcm.len() / 2;
        assert!(
            pcm[mid] < -10000,
            "all-zero DSD should produce negative PCM, got {}",
            pcm[mid]
        );
    }

    #[test]
    fn all_ones_dsd_produces_positive_dc() {
        // All 0xFF = all bits 1 = all +1.0 => should produce large positive PCM values
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let bytes_per_ch = 1024;
        let dsd_data = vec![0xFFu8; bytes_per_ch];
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        let mid = pcm.len() / 2;
        assert!(
            pcm[mid] > 10000,
            "all-ones DSD should produce positive PCM, got {}",
            pcm[mid]
        );
    }

    #[test]
    fn dsd128_conversion() {
        let channels = 2;
        let converter = DsdToPcmConverter::new(5_644_800, 176_400, channels, true);

        assert_eq!(converter.decimation_ratio, 32);
        assert_eq!(converter.output_rate, 176_400);

        let bytes_per_ch = 2048;
        let dsd_data: Vec<u8> = (0..bytes_per_ch * channels).map(|_| 0x55u8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty(), "DSD128 conversion should produce output");
    }

    #[test]
    fn msb_first_dff_conversion() {
        // DFF uses MSB-first bit ordering
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, false);

        let bytes_per_ch = 1024;
        // 0xAA = 10101010 MSB-first => alternating +1/-1 => near silence
        let dsd_data: Vec<u8> = (0..bytes_per_ch).map(|_| 0xAAu8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        // Middle samples should be near zero
        let mid_start = pcm.len() / 4;
        let mid_end = 3 * pcm.len() / 4;
        for &s in &pcm[mid_start..mid_end] {
            assert!(
                s.abs() < 1000,
                "alternating DFF pattern should be near-silence, got {s}"
            );
        }
    }

    #[test]
    fn choose_output_rate_dsd64() {
        assert_eq!(choose_output_rate(2_822_400), 176_400);
    }

    #[test]
    fn choose_output_rate_dsd128() {
        assert_eq!(choose_output_rate(5_644_800), 352_800);
    }

    #[test]
    fn choose_output_rate_dsd256() {
        assert_eq!(choose_output_rate(11_289_600), 352_800);
    }

    #[test]
    fn choose_output_rate_dsd512() {
        assert_eq!(choose_output_rate(22_579_200), 352_800);
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, 2, true);
        let pcm = converter.process(&[]);
        assert!(pcm.is_empty());
    }

    #[test]
    fn stereo_channel_count_preserved() {
        let channels = 2;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        // 256 bytes per channel, interleaved
        let total_bytes = 256 * channels;
        let dsd_data = vec![0x55u8; total_bytes];
        let pcm = converter.process_to_i16(&dsd_data);

        // Output sample count should be divisible by channel count
        assert_eq!(
            pcm.len() % channels,
            0,
            "output samples should be a multiple of channel count"
        );
    }

    #[test]
    fn output_depth_is_24() {
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, 2, true);
        assert_eq!(converter.output_depth, 24);
    }

    #[test]
    fn process_24bit_output_format() {
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let dsd_data = vec![0x55u8; 512];
        let pcm_24 = converter.process(&dsd_data);

        // Each sample is 3 bytes (24-bit)
        assert_eq!(
            pcm_24.len() % 3,
            0,
            "24-bit output should be a multiple of 3 bytes"
        );
    }

    // --- DsdToPcmStreamer tests ---

    #[test]
    fn streamer_produces_output() {
        let channels = 2;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);

        let total_bytes = 2048 * channels;
        let dsd_data: Vec<u8> = (0..total_bytes).map(|_| 0x55u8).collect();

        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();

        let total_pcm = pcm.len() + flush.len();
        assert!(total_pcm > 0, "streamer should produce PCM output");
        assert_eq!(total_pcm % 3, 0, "output should be 24-bit (3 bytes/sample)");
        assert_eq!(
            (total_pcm / 3) % channels,
            0,
            "output should have correct channel count"
        );
    }

    #[test]
    fn streamer_silence_pattern() {
        // Alternating 0x55 pattern = near-silence
        let channels = 1;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);

        let dsd_data = vec![0x55u8; 4096];
        let pcm_24 = streamer.feed(&dsd_data);
        let flush = streamer.flush();

        let mut all_pcm = pcm_24;
        all_pcm.extend_from_slice(&flush);

        // Convert to i16 for easy checking
        let num_samples = all_pcm.len() / 3;
        assert!(num_samples > 10, "should produce at least 10 samples");

        let mut max_abs: i32 = 0;
        for i in num_samples / 4..3 * num_samples / 4 {
            let offset = i * 3;
            let lo = all_pcm[offset] as u32;
            let mid = all_pcm[offset + 1] as u32;
            let hi = all_pcm[offset + 2] as u32;
            let val24 = lo | (mid << 8) | (hi << 16);
            let val32 = if val24 & 0x80_0000 != 0 {
                (val24 | 0xFF00_0000) as i32
            } else {
                val24 as i32
            };
            let val16 = val32 >> 8;
            if val16.abs() > max_abs {
                max_abs = val16.abs();
            }
        }
        assert!(
            max_abs < 1000,
            "alternating DSD pattern should be near-silence, max abs = {max_abs}"
        );
    }

    #[test]
    fn streamer_chunked_matches_single_feed() {
        // Feeding data in multiple small chunks should produce the same output
        // as feeding it all at once.
        let channels = 2;
        let total_bytes = 1024 * channels;
        let dsd_data: Vec<u8> = (0..total_bytes).map(|i| (i % 256) as u8).collect();

        // Single feed
        let mut streamer1 = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let pcm1 = streamer1.feed(&dsd_data);
        let flush1 = streamer1.flush();
        let mut all1 = pcm1;
        all1.extend_from_slice(&flush1);

        // Chunked feed (feed in small chunks)
        let mut streamer2 = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let chunk_size = 64 * channels; // 64 bytes per channel per chunk
        let mut all2 = Vec::new();
        for chunk in dsd_data.chunks(chunk_size) {
            all2.extend_from_slice(&streamer2.feed(chunk));
        }
        all2.extend_from_slice(&streamer2.flush());

        assert_eq!(
            all1.len(),
            all2.len(),
            "single feed and chunked feed should produce same length"
        );
        assert_eq!(
            all1, all2,
            "single feed and chunked feed should produce identical output"
        );
    }

    #[test]
    fn streamer_dsd128() {
        let channels = 2;
        let mut streamer = DsdToPcmStreamer::new(5_644_800, 352_800, channels, true);
        assert_eq!(streamer.output_rate, 352_800);

        let dsd_data = vec![0xAAu8; 4096 * channels];
        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();
        let total = pcm.len() + flush.len();
        assert!(total > 0, "DSD128 streaming should produce output");
    }

    #[test]
    fn streamer_dff_msb_first() {
        let channels = 1;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, false);

        let dsd_data = vec![0xAAu8; 4096];
        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();
        let total = pcm.len() + flush.len();
        assert!(total > 0, "DFF MSB-first streaming should produce output");
    }

    #[test]
    fn streamer_empty_input() {
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, 2, true);
        let pcm = streamer.feed(&[]);
        assert!(pcm.is_empty());
        let flush = streamer.flush();
        assert!(flush.is_empty());
    }

    #[test]
    fn streamer_matches_batch_stereo() {
        // The streaming converter must produce identical output to the batch
        // converter for the same input data.  This catches bugs in the ring
        // buffer indexing (e.g. overwriting channel 0's samples because
        // ring_pos didn't advance between bits within a byte).
        let channels = 2;
        let total_bytes = 512 * channels;
        // Use a deterministic non-trivial pattern (not silence or DC)
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 37 + 13) % 256) as u8)
            .collect();

        // Batch converter
        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        // Streaming converter (single feed)
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let stream_pcm = streamer.feed(&dsd_data);
        let stream_flush = streamer.flush();
        let mut all_stream = stream_pcm;
        all_stream.extend_from_slice(&stream_flush);

        // Both should produce the same number of samples
        assert_eq!(
            batch_pcm.len(),
            all_stream.len(),
            "batch and streaming should produce same byte count"
        );

        // Compare sample by sample (allow tiny rounding differences from f64 arithmetic)
        let num_samples = batch_pcm.len() / 3;
        let mut max_diff: i32 = 0;
        for i in 0..num_samples {
            let off = i * 3;
            let batch_val = {
                let lo = batch_pcm[off] as u32;
                let mid = batch_pcm[off + 1] as u32;
                let hi = batch_pcm[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let stream_val = {
                let lo = all_stream[off] as u32;
                let mid = all_stream[off + 1] as u32;
                let hi = all_stream[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let diff = (batch_val - stream_val).abs();
            if diff > max_diff {
                max_diff = diff;
            }
        }

        assert!(
            max_diff <= 1,
            "batch and streaming outputs should match (max sample diff = {max_diff})"
        );
    }

    #[test]
    fn streamer_fast_path_matches_batch_long() {
        // Longer input so most output samples are emitted in steady state and
        // exercise the contiguous two-slice fast path in `feed`. It must still
        // match the independent batch converter (which uses the plain indexed
        // algorithm), guarding the fast-path ring math.
        let channels = 2;
        let total_bytes = 4096 * channels; // 32768 DSD samples/ch -> ~2048 out/ch
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 101 + 7) % 256) as u8)
            .collect();

        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let mut all_stream = streamer.feed(&dsd_data);
        all_stream.extend_from_slice(&streamer.flush());

        assert_eq!(batch_pcm.len(), all_stream.len());
        let num_samples = batch_pcm.len() / 3;
        assert!(
            num_samples > 2000,
            "should exercise many steady-state samples"
        );

        let read = |b: &[u8], off: usize| -> i32 {
            let v = b[off] as u32 | ((b[off + 1] as u32) << 8) | ((b[off + 2] as u32) << 16);
            if v & 0x80_0000 != 0 {
                (v | 0xFF00_0000) as i32
            } else {
                v as i32
            }
        };
        let mut max_diff = 0i32;
        for i in 0..num_samples {
            let off = i * 3;
            max_diff = max_diff.max((read(&batch_pcm, off) - read(&all_stream, off)).abs());
        }
        assert!(
            max_diff <= 1,
            "fast path must match batch (max diff = {max_diff})"
        );
    }

    #[test]
    fn streamer_matches_batch_mono() {
        // Mono should also match (mono was less affected by the original bug
        // since there's only one channel, but verify for completeness).
        let channels = 1;
        let total_bytes = 512;
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 37 + 13) % 256) as u8)
            .collect();

        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let stream_pcm = streamer.feed(&dsd_data);
        let stream_flush = streamer.flush();
        let mut all_stream = stream_pcm;
        all_stream.extend_from_slice(&stream_flush);

        assert_eq!(batch_pcm.len(), all_stream.len());

        let num_samples = batch_pcm.len() / 3;
        let mut max_diff: i32 = 0;
        for i in 0..num_samples {
            let off = i * 3;
            let batch_val = {
                let lo = batch_pcm[off] as u32;
                let mid = batch_pcm[off + 1] as u32;
                let hi = batch_pcm[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let stream_val = {
                let lo = all_stream[off] as u32;
                let mid = all_stream[off + 1] as u32;
                let hi = all_stream[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let diff = (batch_val - stream_val).abs();
            if diff > max_diff {
                max_diff = diff;
            }
        }

        assert!(
            max_diff <= 1,
            "batch and streaming mono outputs should match (max sample diff = {max_diff})"
        );
    }
}
