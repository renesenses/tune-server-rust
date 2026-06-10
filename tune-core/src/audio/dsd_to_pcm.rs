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

    /// Maximum total allocation (in bytes) that `process()` is allowed to
    /// make for intermediate and output buffers combined.  This prevents
    /// the global allocator from panicking on extreme inputs.
    const MAX_PROCESS_ALLOC: usize = 512 * 1024 * 1024; // 512 MB

    /// Convert DSD data bytes to PCM samples.
    ///
    /// Input: interleaved DSD bytes (ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...)
    /// Each byte contains 8 DSD samples.
    ///
    /// Output: interleaved 24-bit PCM samples as little-endian bytes (3 bytes per sample).
    /// Layout: ch0_sample0 (3 bytes), ch1_sample0 (3 bytes), ch0_sample1 (3 bytes), ...
    ///
    /// Returns an empty Vec if the estimated allocation would exceed the
    /// safety cap (512 MB).
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

        // Memory guard: estimate total allocation before committing.
        // channel_bits: channels * total_dsd_samples * 8 bytes (f64)
        // output: output_samples_per_ch * channels * 3 bytes
        let channel_bits_bytes = channels.saturating_mul(total_dsd_samples).saturating_mul(8);
        let output_bytes = output_samples_per_ch
            .saturating_mul(channels)
            .saturating_mul(3);
        let total_alloc = channel_bits_bytes.saturating_add(output_bytes);

        if total_alloc > Self::MAX_PROCESS_ALLOC {
            tracing::error!(
                total_alloc_mb = total_alloc / (1024 * 1024),
                max_mb = Self::MAX_PROCESS_ALLOC / (1024 * 1024),
                dsd_data_len = dsd_data.len(),
                channels,
                decimation_ratio = self.decimation_ratio,
                "dsd_process_rejected_oom_guard"
            );
            return Vec::new();
        }

        // De-interleave DSD data into per-channel bit streams
        // and expand bytes to +1/-1 sample values for filtering
        let mut channel_bits: Vec<Vec<f64>> = Vec::with_capacity(channels);
        for _ in 0..channels {
            let mut v = Vec::new();
            if v.try_reserve(total_dsd_samples).is_err() {
                tracing::error!(samples = total_dsd_samples, "dsd_channel_bits_alloc_failed");
                return Vec::new();
            }
            channel_bits.push(v);
        }

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
        let out_len = output_samples_per_ch * channels * 3;
        let mut output = Vec::new();
        if output.try_reserve(out_len).is_err() {
            tracing::error!(bytes = out_len, "dsd_output_alloc_failed");
            return Vec::new();
        }

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
/// Returns a rate that's a clean integer divisor of the DSD rate.
/// Capped at 176.4 kHz for ALL DSD rates to prevent excessive memory
/// usage during DSD-to-PCM conversion.  352.8 kHz PCM is perceptually
/// identical and most DACs cannot play it over DLNA/local output anyway.
/// A DSD256 60-minute track at 352.8 kHz / 24-bit / stereo would need
/// ~7.6 GB of PCM — capping to 176.4 kHz halves that to ~3.8 GB, and
/// with the additional memory guard in decode_dsd_to_pcm the server
/// stays well within safe limits.
pub fn choose_output_rate(dsd_rate: u32) -> u32 {
    match dsd_rate {
        r if r >= 2_000_000 => 176_400, // DSD64/128/256/512 → cap at 176.4 kHz
        _ => 176_400,                   // fallback
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
        // Capped to 176.4 kHz to prevent OOM on long DSD tracks
        assert_eq!(choose_output_rate(5_644_800), 176_400);
    }

    #[test]
    fn choose_output_rate_dsd256() {
        assert_eq!(choose_output_rate(11_289_600), 176_400);
    }

    #[test]
    fn choose_output_rate_dsd512() {
        assert_eq!(choose_output_rate(22_579_200), 176_400);
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
}
