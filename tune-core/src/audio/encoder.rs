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

pub struct FFmpegEncoder {
    format: String,
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
    process: Option<Child>,
}

impl FFmpegEncoder {
    pub fn new(format: &str, sample_rate: u32, bit_depth: u32, channels: u32) -> Self {
        Self {
            format: format.to_string(),
            sample_rate,
            bit_depth,
            channels,
            process: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        let in_fmt = pcm_format(self.bit_depth);

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-f".to_string(),
            in_fmt.to_string(),
            "-ar".to_string(),
            self.sample_rate.to_string(),
            "-ac".to_string(),
            self.channels.to_string(),
            "-i".to_string(),
            "pipe:0".to_string(),
        ];

        match self.format.as_str() {
            "flac" => {
                args.extend(["-f".into(), "flac".into()]);
            }
            "mp3" => {
                args.extend(["-codec:a".into(), "libmp3lame".into(), "-q:a".into(), "2".into(), "-f".into(), "mp3".into()]);
            }
            "wav" => {
                args.extend(["-f".into(), "wav".into()]);
            }
            "ogg" => {
                args.extend(["-codec:a".into(), "libvorbis".into(), "-q:a".into(), "6".into(), "-f".into(), "ogg".into()]);
            }
            other => {
                args.extend(["-f".into(), other.to_string()]);
            }
        }

        args.push("pipe:1".into());

        debug!(
            format = self.format,
            sample_rate = self.sample_rate,
            "encoder_start"
        );

        let child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("encoder spawn: {e}"))?;

        self.process = Some(child);
        Ok(())
    }

    pub async fn write(&mut self, pcm_data: &[u8]) -> Result<(), String> {
        let child = self.process.as_mut().ok_or("encoder not started")?;
        let stdin = child.stdin.as_mut().ok_or("no stdin")?;
        stdin
            .write_all(pcm_data)
            .await
            .map_err(|e| format!("encoder write: {e}"))?;
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        if let Some(mut child) = self.process.take() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.shutdown().await;
            }

            let output = child
                .wait_with_output()
                .await
                .map_err(|e| format!("encoder finish: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(error = %stderr, "encoder_error");
            }

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

impl Drop for FFmpegEncoder {
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
    fn pcm_format_mapping() {
        assert_eq!(pcm_format(16), "s16le");
        assert_eq!(pcm_format(24), "s24le");
        assert_eq!(pcm_format(32), "s32le");
    }

    #[test]
    fn encoder_new() {
        let enc = FFmpegEncoder::new("flac", 44100, 16, 2);
        assert_eq!(enc.format, "flac");
        assert_eq!(enc.sample_rate, 44100);
        assert!(enc.process.is_none());
    }

    #[tokio::test]
    async fn finish_without_start() {
        let mut enc = FFmpegEncoder::new("flac", 44100, 16, 2);
        let result = enc.finish().await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn stop_without_start() {
        let mut enc = FFmpegEncoder::new("flac", 44100, 16, 2);
        enc.stop().await;
        assert!(enc.process.is_none());
    }
}
