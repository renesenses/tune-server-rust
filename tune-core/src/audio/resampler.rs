use rubato::{FftFixedIn, Resampler as RubatoResampler};
use tracing::debug;

#[allow(dead_code)]
fn pcm_format(depth: u32) -> &'static str {
    match depth {
        0..=16 => "s16le",
        17..=24 => "s24le",
        _ => "s32le",
    }
}

pub struct Resampler {
    source_rate: u32,
    target_rate: u32,
    source_depth: u32,
    target_depth: u32,
    channels: u32,
    needs_resample: bool,
    resampler: Option<FftFixedIn<f64>>,
}

impl Resampler {
    pub fn new(
        source_rate: u32,
        target_rate: u32,
        source_depth: u32,
        target_depth: u32,
        channels: u32,
    ) -> Self {
        let actual_target_rate = target_rate.min(source_rate);
        let actual_target_depth = target_depth.min(source_depth);
        let needs = actual_target_rate != source_rate || actual_target_depth != source_depth;
        Self {
            source_rate,
            target_rate: actual_target_rate,
            source_depth,
            target_depth: actual_target_depth,
            channels,
            needs_resample: needs,
            resampler: None,
        }
    }

    pub fn needs_resample(&self) -> bool {
        self.needs_resample
    }

    pub fn output_rate(&self) -> u32 {
        self.target_rate
    }

    pub fn output_depth(&self) -> u32 {
        self.target_depth
    }

    pub async fn start(&mut self) -> Result<(), String> {
        if !self.needs_resample {
            return Ok(());
        }

        debug!(
            source_rate = self.source_rate,
            target_rate = self.target_rate,
            channels = self.channels,
            "resampler_start_rubato"
        );

        let chunk_size = 1024;
        let resampler = FftFixedIn::<f64>::new(
            self.source_rate as usize,
            self.target_rate as usize,
            chunk_size,
            2,
            self.channels as usize,
        )
        .map_err(|e| format!("rubato init: {e}"))?;

        self.resampler = Some(resampler);
        Ok(())
    }

    pub async fn process_chunk(&mut self, pcm_data: &[u8]) -> Result<Vec<u8>, String> {
        if !self.needs_resample {
            return Ok(pcm_data.to_vec());
        }

        let resampler = self.resampler.as_mut().ok_or("resampler not started")?;
        let bytes_per_sample = (self.source_depth as usize + 7) / 8;
        let channels = self.channels as usize;
        let frames = pcm_data.len() / (bytes_per_sample * channels);

        if frames == 0 {
            return Ok(Vec::new());
        }

        // Deinterleave PCM bytes → Vec<Vec<f64>> per channel
        let mut channel_data: Vec<Vec<f64>> = vec![Vec::with_capacity(frames); channels];
        for frame in 0..frames {
            for ch in 0..channels {
                let offset = (frame * channels + ch) * bytes_per_sample;
                let sample = match bytes_per_sample {
                    2 => {
                        let s = i16::from_le_bytes([pcm_data[offset], pcm_data[offset + 1]]);
                        s as f64 / 32768.0
                    }
                    3 => {
                        let s = i32::from_le_bytes([
                            pcm_data[offset],
                            pcm_data[offset + 1],
                            pcm_data[offset + 2],
                            if pcm_data[offset + 2] & 0x80 != 0 {
                                0xFF
                            } else {
                                0
                            },
                        ]);
                        s as f64 / 8388608.0
                    }
                    4 => {
                        let s = i32::from_le_bytes([
                            pcm_data[offset],
                            pcm_data[offset + 1],
                            pcm_data[offset + 2],
                            pcm_data[offset + 3],
                        ]);
                        s as f64 / 2147483648.0
                    }
                    _ => 0.0,
                };
                channel_data[ch].push(sample);
            }
        }

        // Resample
        let resampled = resampler
            .process(&channel_data, None)
            .map_err(|e| format!("rubato process: {e}"))?;

        // Interleave back to PCM bytes
        let out_bytes_per_sample = (self.target_depth as usize + 7) / 8;
        let out_frames = resampled[0].len();
        let mut output = Vec::with_capacity(out_frames * channels * out_bytes_per_sample);

        for frame in 0..out_frames {
            for ch in 0..channels {
                let sample = resampled[ch][frame];
                match out_bytes_per_sample {
                    2 => {
                        let s = (sample * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
                        output.extend_from_slice(&s.to_le_bytes());
                    }
                    3 => {
                        let s = (sample * 8388607.0).round().clamp(-8388608.0, 8388607.0) as i32;
                        let bytes = s.to_le_bytes();
                        output.extend_from_slice(&bytes[..3]);
                    }
                    4 => {
                        let s = (sample * 2147483647.0)
                            .round()
                            .clamp(-2147483648.0, 2147483647.0)
                            as i32;
                        output.extend_from_slice(&s.to_le_bytes());
                    }
                    _ => {}
                }
            }
        }

        Ok(output)
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        if !self.needs_resample {
            return Ok(Vec::new());
        }
        self.resampler = None;
        Ok(Vec::new())
    }

    pub async fn stop(&mut self) {
        self.resampler = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_upsamples() {
        let r = Resampler::new(44100, 96000, 16, 24, 2);
        assert_eq!(r.output_rate(), 44100);
        assert_eq!(r.output_depth(), 16);
        assert!(!r.needs_resample());
    }

    #[test]
    fn downsamples_when_needed() {
        let r = Resampler::new(96000, 44100, 24, 16, 2);
        assert_eq!(r.output_rate(), 44100);
        assert_eq!(r.output_depth(), 16);
        assert!(r.needs_resample());
    }

    #[test]
    fn same_rate_no_resample() {
        let r = Resampler::new(44100, 44100, 16, 16, 2);
        assert!(!r.needs_resample());
    }

    #[test]
    fn pcm_format_mapping() {
        assert_eq!(pcm_format(16), "s16le");
        assert_eq!(pcm_format(24), "s24le");
        assert_eq!(pcm_format(32), "s32le");
        assert_eq!(pcm_format(8), "s16le");
    }

    #[tokio::test]
    async fn passthrough_when_no_resample() {
        let mut r = Resampler::new(44100, 44100, 16, 16, 2);
        let data = vec![1u8, 2, 3, 4];
        let out = r.process_chunk(&data).await.unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn finish_when_no_resample() {
        let mut r = Resampler::new(44100, 44100, 16, 16, 2);
        let remaining = r.finish().await.unwrap();
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn resample_produces_output() {
        let mut r = Resampler::new(96000, 44100, 16, 16, 2);
        r.start().await.unwrap();
        // 1024 frames of silence (stereo 16-bit = 4096 bytes)
        let data = vec![0u8; 1024 * 2 * 2];
        let out = r.process_chunk(&data).await.unwrap();
        // Output should be shorter (downsampled)
        assert!(!out.is_empty());
        assert!(out.len() < data.len());
    }
}
