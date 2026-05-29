use tracing::debug;

pub struct PcmMixer {
    channels: u16,
    bit_depth: u16,
    sample_rate: u32,
}

impl PcmMixer {
    pub fn new(channels: u16, bit_depth: u16, sample_rate: u32) -> Self {
        Self {
            channels,
            bit_depth,
            sample_rate,
        }
    }

    pub fn mix_buffers(&self, buffers: &[&[u8]], gains: &[f32]) -> Vec<u8> {
        if buffers.is_empty() {
            return Vec::new();
        }

        let max_len = buffers.iter().map(|b| b.len()).max().unwrap_or(0);

        match self.bit_depth {
            16 => self.mix_16bit(buffers, gains, max_len),
            24 => self.mix_24bit(buffers, gains, max_len),
            _ => buffers.first().map(|b| b.to_vec()).unwrap_or_default(),
        }
    }

    fn mix_16bit(&self, buffers: &[&[u8]], gains: &[f32], max_len: usize) -> Vec<u8> {
        let sample_count = max_len / 2;
        let mut output = vec![0u8; sample_count * 2];

        for i in 0..sample_count {
            let mut sum: f64 = 0.0;
            for (buf_idx, buf) in buffers.iter().enumerate() {
                let gain = gains.get(buf_idx).copied().unwrap_or(1.0) as f64;
                let byte_pos = i * 2;
                if byte_pos + 1 < buf.len() {
                    let sample = i16::from_le_bytes([buf[byte_pos], buf[byte_pos + 1]]);
                    sum += sample as f64 * gain;
                }
            }

            let clamped = sum.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            let bytes = clamped.to_le_bytes();
            output[i * 2] = bytes[0];
            output[i * 2 + 1] = bytes[1];
        }

        output
    }

    fn mix_24bit(&self, buffers: &[&[u8]], gains: &[f32], max_len: usize) -> Vec<u8> {
        let sample_count = max_len / 3;
        let mut output = vec![0u8; sample_count * 3];

        for i in 0..sample_count {
            let mut sum: f64 = 0.0;
            for (buf_idx, buf) in buffers.iter().enumerate() {
                let gain = gains.get(buf_idx).copied().unwrap_or(1.0) as f64;
                let byte_pos = i * 3;
                if byte_pos + 2 < buf.len() {
                    let raw = ((buf[byte_pos + 2] as i32) << 16)
                        | ((buf[byte_pos + 1] as i32) << 8)
                        | (buf[byte_pos] as i32);
                    let sample = if raw & 0x800000 != 0 {
                        raw | !0xFFFFFF
                    } else {
                        raw
                    };
                    sum += sample as f64 * gain;
                }
            }

            let clamped = sum.clamp(-8388608.0, 8388607.0) as i32;
            output[i * 3] = (clamped & 0xFF) as u8;
            output[i * 3 + 1] = ((clamped >> 8) & 0xFF) as u8;
            output[i * 3 + 2] = ((clamped >> 16) & 0xFF) as u8;
        }

        output
    }

    pub fn apply_gain(data: &mut [u8], gain: f32, bit_depth: u16) {
        match bit_depth {
            16 => {
                for chunk in data.chunks_exact_mut(2) {
                    let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    let adjusted =
                        (sample as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    let bytes = adjusted.to_le_bytes();
                    chunk[0] = bytes[0];
                    chunk[1] = bytes[1];
                }
            }
            24 => {
                for chunk in data.chunks_exact_mut(3) {
                    let raw =
                        ((chunk[2] as i32) << 16) | ((chunk[1] as i32) << 8) | (chunk[0] as i32);
                    let sample = if raw & 0x800000 != 0 {
                        raw | !0xFFFFFF
                    } else {
                        raw
                    };
                    let adjusted = (sample as f32 * gain).clamp(-8388608.0, 8388607.0) as i32;
                    chunk[0] = (adjusted & 0xFF) as u8;
                    chunk[1] = ((adjusted >> 8) & 0xFF) as u8;
                    chunk[2] = ((adjusted >> 16) & 0xFF) as u8;
                }
            }
            _ => {}
        }
    }

    pub fn silence(&self, duration_ms: u64) -> Vec<u8> {
        let sample_count = (self.sample_rate as u64 * self.channels as u64 * duration_ms) / 1000;
        let bytes_per_sample = (self.bit_depth / 8) as u64;
        vec![0u8; (sample_count * bytes_per_sample) as usize]
    }

    pub fn duration_ms(&self, data_len: usize) -> u64 {
        let bytes_per_sample = (self.bit_depth / 8) as u64;
        let total_samples = data_len as u64 / bytes_per_sample;
        let frames = total_samples / self.channels as u64;
        (frames * 1000) / self.sample_rate as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_two_16bit_buffers() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 2000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]);
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, 3000);
    }

    #[test]
    fn mix_with_gain() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 1000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[0.5, 0.5]);
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, 1000);
    }

    #[test]
    fn clamp_on_overflow() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 30000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 30000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]);
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, i16::MAX);
    }

    #[test]
    fn apply_gain_16bit() {
        let mut data: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        PcmMixer::apply_gain(&mut data, 0.5, 16);
        let result = i16::from_le_bytes([data[0], data[1]]);
        assert_eq!(result, 500);
    }

    #[test]
    fn silence_duration() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let silence = mixer.silence(1000);
        assert_eq!(silence.len(), 44100 * 2 * 2);
        assert!(silence.iter().all(|&b| b == 0));
    }

    #[test]
    fn duration_calculation() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let data_len = 44100 * 2 * 2;
        assert_eq!(mixer.duration_ms(data_len), 1000);
    }

    #[test]
    fn empty_mix() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let result = mixer.mix_buffers(&[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn mix_24bit_basic() {
        let mixer = PcmMixer::new(1, 24, 44100);
        let buf1 = vec![0x00u8, 0x10, 0x00]; // 4096
        let buf2 = vec![0x00u8, 0x10, 0x00]; // 4096

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]);
        let val = (mixed[0] as i32) | ((mixed[1] as i32) << 8) | ((mixed[2] as i32) << 16);
        assert_eq!(val, 8192);
    }
}
