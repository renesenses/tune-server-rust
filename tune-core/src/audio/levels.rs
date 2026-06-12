#[derive(Debug, Clone, Default)]
pub struct AudioLevels {
    pub rms_left: f64,
    pub rms_right: f64,
    pub peak_left: f64,
    pub peak_right: f64,
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
