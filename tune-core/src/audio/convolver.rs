use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Partitioned overlap-save FFT convolver for real-time FIR filtering.
///
/// Processes audio in fixed-size blocks using FFT convolution.
/// Supports arbitrary-length impulse responses by partitioning them
/// into segments and accumulating the results.
pub struct Convolver {
    block_size: usize,
    fft_size: usize,
    channels: usize,
    /// FFT of each partition of the impulse response, per channel.
    ir_partitions: Vec<Vec<Vec<Complex<f32>>>>,
    /// Input buffer per channel (collects samples until block_size is reached).
    input_buf: Vec<Vec<f32>>,
    /// Frequency-domain delay line per channel (one slot per partition).
    fdl: Vec<Vec<Vec<Complex<f32>>>>,
    fdl_pos: usize,
    /// Overlap buffer per channel (tail from previous block).
    overlap: Vec<Vec<f32>>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
}

impl Convolver {
    pub fn new(impulse_response: &[Vec<f32>], block_size: usize) -> Self {
        let channels = impulse_response.len();
        assert!(channels > 0, "IR must have at least one channel");
        let fft_size = (block_size * 2).next_power_of_two();
        let spectrum_len = fft_size / 2 + 1;

        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(fft_size);
        let inv = planner.plan_fft_inverse(fft_size);

        let mut ir_partitions = Vec::with_capacity(channels);
        let mut num_partitions = 0;

        for ch_ir in impulse_response {
            let n_parts = (ch_ir.len() + block_size - 1) / block_size;
            num_partitions = num_partitions.max(n_parts);
            let mut ch_parts = Vec::with_capacity(n_parts);

            for p in 0..n_parts {
                let start = p * block_size;
                let end = (start + block_size).min(ch_ir.len());
                let mut padded = vec![0.0f32; fft_size];
                padded[..end - start].copy_from_slice(&ch_ir[start..end]);
                let mut spectrum = fwd.make_output_vec();
                fwd.process(&mut padded, &mut spectrum).unwrap();
                ch_parts.push(spectrum);
            }
            ir_partitions.push(ch_parts);
        }

        let zero_spectrum = vec![Complex::new(0.0, 0.0); spectrum_len];
        let fdl: Vec<Vec<Vec<_>>> = (0..channels)
            .map(|_| vec![zero_spectrum.clone(); num_partitions])
            .collect();

        let input_buf = vec![Vec::with_capacity(block_size); channels];
        let overlap = vec![vec![0.0f32; fft_size - block_size]; channels];

        Self {
            block_size,
            fft_size,
            channels,
            ir_partitions,
            input_buf,
            fdl,
            fdl_pos: 0,
            overlap,
            fwd,
            inv,
        }
    }

    /// Load impulse response from a WAV file.
    pub fn from_wav(path: &str, block_size: usize) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("read IR: {e}"))?;
        if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }

        // Walk the RIFF chunks to locate `fmt ` and `data` at their REAL
        // offsets. Impulse WAVs exported by REW / rePhase / Dirac often carry
        // extra chunks (fact, PEAK, LIST/INFO, cue…) before `data`, so a fixed
        // 44-byte header offset misreads them into noise.
        let mut format_tag: u16 = 1; // 1 = PCM int, 3 = IEEE float
        let mut channels: usize = 0;
        let mut sample_rate: u32 = 0;
        let mut bits: usize = 0;
        let mut data_range: Option<(usize, usize)> = None;
        let mut pos = 12usize;
        while pos + 8 <= data.len() {
            let id = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
            let declared =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let body = pos + 8;
            // Clamp to what's actually present so a truncated final chunk can't
            // index out of bounds.
            let size = declared.min(data.len() - body);
            match &id {
                b"fmt " if size >= 16 => {
                    format_tag = u16::from_le_bytes([data[body], data[body + 1]]);
                    channels = u16::from_le_bytes([data[body + 2], data[body + 3]]) as usize;
                    sample_rate = u32::from_le_bytes([
                        data[body + 4],
                        data[body + 5],
                        data[body + 6],
                        data[body + 7],
                    ]);
                    bits = u16::from_le_bytes([data[body + 14], data[body + 15]]) as usize;
                    // WAVE_FORMAT_EXTENSIBLE: the effective format tag lives in
                    // the first 2 bytes of the sub-format GUID.
                    if format_tag == 0xFFFE && size >= 40 {
                        format_tag = u16::from_le_bytes([data[body + 24], data[body + 25]]);
                    }
                }
                b"data" => data_range = Some((body, body + size)),
                _ => {}
            }
            // Chunks are word-aligned: an odd size carries a trailing pad byte.
            pos = body + size + (size & 1);
        }

        let (dstart, dend) = data_range.ok_or("missing data chunk")?;
        if channels == 0 {
            return Err("missing or invalid fmt chunk".into());
        }
        let bytes_per_sample = bits / 8;
        if bytes_per_sample == 0 {
            return Err(format!("unsupported bit depth: {bits}"));
        }
        let is_float = format_tag == 3;

        let total_samples = (dend - dstart) / bytes_per_sample;
        let samples_per_channel = total_samples / channels;
        if samples_per_channel == 0 {
            return Err("IR has no samples".into());
        }

        tracing::info!(
            path,
            channels,
            sample_rate,
            bits,
            is_float,
            samples_per_channel,
            "convolver_ir_loaded"
        );

        let mut ir = vec![Vec::with_capacity(samples_per_channel); channels];
        for i in 0..samples_per_channel {
            for ch in 0..channels {
                let o = dstart + (i * channels + ch) * bytes_per_sample;
                let sample = match (bits, is_float) {
                    (16, false) => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
                    (24, false) => {
                        // 24-bit LE placed in the top 3 bytes of an i32 for
                        // correct sign extension, then scaled by 2^31.
                        let v = i32::from_le_bytes([0, data[o], data[o + 1], data[o + 2]]);
                        v as f32 / 2147483648.0
                    }
                    (32, false) => {
                        let v =
                            i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
                        v as f32 / 2147483648.0
                    }
                    (32, true) => {
                        f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
                    }
                    (64, true) => f64::from_le_bytes([
                        data[o],
                        data[o + 1],
                        data[o + 2],
                        data[o + 3],
                        data[o + 4],
                        data[o + 5],
                        data[o + 6],
                        data[o + 7],
                    ]) as f32,
                    _ => {
                        return Err(format!(
                            "unsupported WAV sample format: {bits}-bit {}",
                            if is_float { "float" } else { "int" }
                        ));
                    }
                };
                ir[ch].push(sample);
            }
        }

        Ok(Self::new(&ir, block_size))
    }

    /// Process interleaved f32 samples in-place.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        let ch = self.channels;
        let frame_count = samples.len() / ch;

        for frame in 0..frame_count {
            for c in 0..ch {
                self.input_buf[c].push(samples[frame * ch + c]);
            }

            if self.input_buf[0].len() >= self.block_size {
                let mut output = vec![vec![0.0f32; self.block_size]; ch];
                self.process_block(&mut output);

                let start_frame = frame + 1 - self.block_size;
                for f in 0..self.block_size {
                    for c in 0..ch {
                        if start_frame + f < frame_count {
                            samples[(start_frame + f) * ch + c] = output[c][f];
                        }
                    }
                }
                for c in 0..ch {
                    self.input_buf[c].drain(..self.block_size);
                }
            }
        }
    }

    fn process_block(&mut self, output: &mut [Vec<f32>]) {
        let spectrum_len = self.fft_size / 2 + 1;
        let num_partitions = self.ir_partitions[0].len();

        for ch in 0..self.channels {
            let mut padded = vec![0.0f32; self.fft_size];
            let n = self.input_buf[ch].len().min(self.block_size);
            padded[..n].copy_from_slice(&self.input_buf[ch][..n]);

            let mut input_spectrum = self.fwd.make_output_vec();
            self.fwd.process(&mut padded, &mut input_spectrum).unwrap();

            self.fdl[ch][self.fdl_pos] = input_spectrum;

            let mut acc = vec![Complex::new(0.0, 0.0); spectrum_len];
            for p in 0..num_partitions {
                let fdl_idx = (self.fdl_pos + num_partitions - p) % num_partitions;
                if p < self.ir_partitions[ch].len() {
                    for k in 0..spectrum_len {
                        acc[k] += self.fdl[ch][fdl_idx][k] * self.ir_partitions[ch][p][k];
                    }
                }
            }

            let mut time_out = self.inv.make_output_vec();
            self.inv.process(&mut acc, &mut time_out).unwrap();

            let scale = 1.0 / self.fft_size as f32;
            for i in 0..self.block_size {
                output[ch][i] = time_out[i] * scale + self.overlap[ch][i];
            }
            let overlap_len = self.fft_size - self.block_size;
            for i in 0..overlap_len {
                self.overlap[ch][i] = time_out[self.block_size + i] * scale;
            }
        }

        self.fdl_pos = (self.fdl_pos + 1) % num_partitions.max(1);
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ir() {
        let ir = vec![vec![1.0, 0.0, 0.0, 0.0]];
        let mut conv = Convolver::new(&ir, 4);
        let mut samples = vec![1.0, 0.5, 0.25, 0.125];
        conv.process_interleaved(&mut samples);
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                (s - [1.0, 0.5, 0.25, 0.125][i]).abs() < 0.001,
                "sample {i}: {s}"
            );
        }
    }

    #[test]
    fn stereo_ir() {
        let ir = vec![vec![1.0, 0.0]; 2];
        let mut conv = Convolver::new(&ir, 4);
        let mut samples = vec![1.0, 0.5, 0.25, 0.125, 1.0, 0.5, 0.25, 0.125];
        conv.process_interleaved(&mut samples);
        assert!((samples[0] - 1.0).abs() < 0.01);
        assert!((samples[1] - 0.5).abs() < 0.01);
    }

    #[test]
    fn from_wav_skips_extra_chunks_before_data() {
        // Identity IR (4 taps, 16-bit mono @48k) with a spurious LIST chunk
        // inserted before `data` — exactly what the old fixed-44-byte parser
        // misread into noise. The chunk-walking parser must still find `data`.
        let ir_i16: [i16; 4] = [32767, 0, 0, 0];
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0, 0, 0, 0]); // RIFF size — patched below
        wav.extend_from_slice(b"WAVE");
        // fmt  (PCM)
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels
        wav.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&(48_000u32 * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        // spurious chunk before data
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(b"INFO");
        // data
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&((ir_i16.len() * 2) as u32).to_le_bytes());
        for s in ir_i16 {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        let path = std::env::temp_dir().join("tune_ir_extrachunks_test.wav");
        std::fs::write(&path, &wav).unwrap();
        let mut conv = Convolver::from_wav(path.to_str().unwrap(), 4).unwrap();
        std::fs::remove_file(&path).ok();

        let mut samples = vec![1.0, 0.5, 0.25, 0.125];
        conv.process_interleaved(&mut samples);
        for (i, &s) in samples.iter().enumerate() {
            assert!(
                (s - [1.0, 0.5, 0.25, 0.125][i]).abs() < 0.01,
                "sample {i}: {s}"
            );
        }
    }
}
