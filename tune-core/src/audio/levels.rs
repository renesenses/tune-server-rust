#[derive(Debug, Clone, Default)]
pub struct AudioLevels {
    pub rms_left: f64,
    pub rms_right: f64,
    pub peak_left: f64,
    pub peak_right: f64,
    pub spectrum: Vec<f32>,
}

impl AudioLevels {
    pub fn rms_left_db(&self) -> f32 {
        to_db(self.rms_left)
    }
    pub fn rms_right_db(&self) -> f32 {
        to_db(self.rms_right)
    }
    pub fn peak_left_db(&self) -> f32 {
        to_db(self.peak_left)
    }
    pub fn peak_right_db(&self) -> f32 {
        to_db(self.peak_right)
    }
}

fn to_db(linear: f64) -> f32 {
    if linear <= 0.0 {
        -96.0
    } else {
        (20.0 * linear.log10()).max(-96.0) as f32
    }
}

pub fn compute_levels(pcm: &[u8], bit_depth: u16, channels: u16) -> AudioLevels {
    if pcm.is_empty() || channels == 0 {
        return AudioLevels::default();
    }

    let bytes_per_sample = (bit_depth / 8) as usize;
    let frame_size = bytes_per_sample * channels as usize;
    if frame_size == 0 {
        return AudioLevels::default();
    }

    let mut sum_sq_l: f64 = 0.0;
    let mut sum_sq_r: f64 = 0.0;
    let mut peak_l: f64 = 0.0;
    let mut peak_r: f64 = 0.0;
    let mut frames: usize = 0;

    let stereo = channels >= 2;

    for frame in pcm.chunks_exact(frame_size) {
        let left = read_sample(frame, 0, bytes_per_sample, bit_depth);
        let right = if stereo {
            read_sample(frame, bytes_per_sample, bytes_per_sample, bit_depth)
        } else {
            left
        };

        sum_sq_l += left * left;
        sum_sq_r += right * right;
        peak_l = peak_l.max(left.abs());
        peak_r = peak_r.max(right.abs());
        frames += 1;
    }

    if frames == 0 {
        return AudioLevels::default();
    }

    AudioLevels {
        rms_left: (sum_sq_l / frames as f64).sqrt(),
        rms_right: (sum_sq_r / frames as f64).sqrt(),
        peak_left: peak_l,
        peak_right: peak_r,
        spectrum: compute_spectrum(pcm, bit_depth, channels, 32),
    }
}

fn read_sample(frame: &[u8], offset: usize, bytes: usize, bit_depth: u16) -> f64 {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    let raw = match bytes {
        2 => {
            let b = [frame[offset], frame[offset + 1]];
            i16::from_le_bytes(b) as f64
        }
        3 => {
            let val = frame[offset] as i32
                | (frame[offset + 1] as i32) << 8
                | ((frame[offset + 2] as i8) as i32) << 16;
            val as f64
        }
        4 => {
            let b = [
                frame[offset],
                frame[offset + 1],
                frame[offset + 2],
                frame[offset + 3],
            ];
            i32::from_le_bytes(b) as f64
        }
        _ => 0.0,
    };
    raw / max_val
}

/// Compute spectrum bins from PCM data using a simple FFT.
/// Returns `bins` magnitude values (0.0..1.0) spread across the frequency range.
pub fn compute_spectrum(pcm: &[u8], bit_depth: u16, channels: u16, bins: usize) -> Vec<f32> {
    if pcm.is_empty() || channels == 0 || bins == 0 {
        return vec![0.0; bins];
    }

    let bytes_per_sample = (bit_depth / 8) as usize;
    let frame_size = bytes_per_sample * channels as usize;
    if frame_size == 0 {
        return vec![0.0; bins];
    }

    // Extract mono samples (mix L+R), max 2048 samples for FFT
    let fft_size = 2048usize;
    let mut samples: Vec<f64> = Vec::with_capacity(fft_size);
    for frame in pcm.chunks_exact(frame_size).take(fft_size) {
        let left = read_sample(frame, 0, bytes_per_sample, bit_depth);
        let right = if channels >= 2 {
            read_sample(frame, bytes_per_sample, bytes_per_sample, bit_depth)
        } else {
            left
        };
        samples.push((left + right) * 0.5);
    }

    let n = samples.len().next_power_of_two().min(fft_size);
    samples.resize(n, 0.0);

    // Apply Hann window
    for i in 0..n {
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
        samples[i] *= w;
    }

    // In-place Cooley-Tukey FFT
    let mut re = samples;
    let mut im = vec![0.0f64; n];

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            re.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j >= m {
            j -= m;
            m >>= 1;
        }
        j += m;
    }

    // FFT butterfly
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let angle_step = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let angle = angle_step * k as f64;
                let wr = angle.cos();
                let wi = angle.sin();
                let a = start + k;
                let b = start + k + half;
                let tr = wr * re[b] - wi * im[b];
                let ti = wr * im[b] + wi * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }

    // Compute magnitudes for the first half (positive frequencies)
    let half = n / 2;
    let mut mags: Vec<f64> = Vec::with_capacity(half);
    let mut max_mag: f64 = 1e-10;
    for i in 0..half {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt();
        max_mag = max_mag.max(mag);
        mags.push(mag);
    }

    // Map FFT bins to output bins (log-scale for perceptual frequency distribution)
    let mut result = vec![0.0f32; bins];
    for b in 0..bins {
        // Log-scale mapping: low bins get more FFT resolution (bass), high bins less (treble)
        let f_low = ((b as f64 / bins as f64).powf(2.0) * half as f64) as usize;
        let f_high = (((b + 1) as f64 / bins as f64).powf(2.0) * half as f64) as usize;
        let f_low = f_low.min(half - 1);
        let f_high = f_high.max(f_low + 1).min(half);

        let mut sum = 0.0;
        let count = (f_high - f_low).max(1);
        for i in f_low..f_high {
            sum += mags[i];
        }
        let avg = sum / count as f64;
        // Normalize to 0..1, apply some compression
        let normalized = (avg / max_mag).powf(0.6);
        result[b] = normalized as f32;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_returns_low_db() {
        let pcm = vec![0u8; 1024];
        let levels = compute_levels(&pcm, 16, 2);
        assert!(levels.rms_left_db() <= -96.0);
        assert!(levels.peak_left_db() <= -96.0);
    }

    #[test]
    fn full_scale_returns_zero_db() {
        let mut pcm = Vec::new();
        for _ in 0..100 {
            pcm.extend_from_slice(&i16::MAX.to_le_bytes()); // left
            pcm.extend_from_slice(&i16::MAX.to_le_bytes()); // right
        }
        let levels = compute_levels(&pcm, 16, 2);
        assert!(levels.peak_left_db() > -1.0);
        assert!(levels.peak_right_db() > -1.0);
    }
}
