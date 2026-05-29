use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tracing::{debug, warn};

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
    process: Option<Child>,
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
            process: None,
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

        let in_fmt = pcm_format(self.source_depth);
        let out_fmt = pcm_format(self.target_depth);
        let af = format!(
            "aresample=resampler=soxr:out_sample_rate={}",
            self.target_rate
        );

        debug!(
            source_rate = self.source_rate,
            target_rate = self.target_rate,
            "resampler_start"
        );

        let child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                in_fmt,
                "-ar",
                &self.source_rate.to_string(),
                "-ac",
                &self.channels.to_string(),
                "-i",
                "pipe:0",
                "-af",
                &af,
                "-f",
                out_fmt,
                "-ac",
                &self.channels.to_string(),
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("resampler spawn: {e}"))?;

        self.process = Some(child);
        Ok(())
    }

    pub async fn process_chunk(&mut self, pcm_data: &[u8]) -> Result<Vec<u8>, String> {
        if !self.needs_resample {
            return Ok(pcm_data.to_vec());
        }

        let child = self.process.as_mut().ok_or("resampler not started")?;
        let stdin = child.stdin.as_mut().ok_or("no stdin")?;
        stdin
            .write_all(pcm_data)
            .await
            .map_err(|e| format!("resampler write: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("resampler flush: {e}"))?;

        let stdout = child.stdout.as_mut().ok_or("no stdout")?;
        let mut buf = vec![0u8; pcm_data.len()];
        let n = tokio::io::AsyncReadExt::read(stdout, &mut buf)
            .await
            .map_err(|e| format!("resampler read: {e}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        if !self.needs_resample {
            return Ok(Vec::new());
        }

        if let Some(mut child) = self.process.take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.shutdown().await;
            }

            let output = child
                .wait_with_output()
                .await
                .map_err(|e| format!("resampler finish: {e}"))?;

            Ok(output.stdout)
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn stop(&mut self) {
        if let Some(ref mut child) = self.process {
            let _ = child.kill().await;
        }
        self.process = None;
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.process {
            let _ = child.start_kill();
        }
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
}
