use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::formats::AudioFormat;

const CHUNK_SIZE: usize = 32 * 1024; // 32 KB chunks

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub file_path: String,
    pub output_format: AudioFormat,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub seek_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub mime_type: String,
}

pub struct AudioPipeline {
    child: Option<Child>,
    tx: mpsc::Sender<Vec<u8>>,
    config: PipelineConfig,
}

impl AudioPipeline {
    pub fn new(config: PipelineConfig, buffer_size: usize) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            Self {
                child: None,
                tx,
                config,
            },
            rx,
        )
    }

    pub fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            format: self.config.output_format,
            sample_rate: self.config.sample_rate,
            bit_depth: self.config.bit_depth,
            channels: self.config.channels,
            mime_type: self.config.output_format.mime_type().to_string(),
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        let ffmpeg = find_ffmpeg().ok_or("FFmpeg not found")?;
        let cfg = &self.config;

        let mut args = vec![
            "-hide_banner".to_string(),
            "-loglevel".into(),
            "warning".into(),
        ];

        if let Some(seek) = cfg.seek_ms {
            let secs = seek as f64 / 1000.0;
            args.extend(["-ss".into(), format!("{secs:.3}")]);
        }

        // DSD requires explicit input format for FFmpeg
        let ext = std::path::Path::new(&cfg.file_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "dsf" => args.extend(["-f".into(), "dsf".into()]),
            "dff" => args.extend(["-f".into(), "dff".into()]),
            _ => {}
        }

        let codec = if cfg.output_format == AudioFormat::Wav {
            match cfg.bit_depth {
                24 => "pcm_s24le",
                32 => "pcm_s32le",
                _ => "pcm_s16le",
            }
        } else {
            cfg.output_format.ffmpeg_codec_arg()
        };

        args.extend([
            "-i".into(),
            cfg.file_path.clone(),
            "-vn".into(),
            "-f".into(),
            cfg.output_format.ffmpeg_format_arg().into(),
            "-acodec".into(),
            codec.into(),
            "-ar".into(),
            cfg.sample_rate.to_string(),
            "-ac".into(),
            cfg.channels.to_string(),
            "pipe:1".into(),
        ]);

        info!(file = %cfg.file_path, format = ?cfg.output_format, "pipeline_start");

        let mut child = Command::new(&ffmpeg)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("FFmpeg spawn failed: {e}"))?;

        let stdout = child.stdout.take().ok_or("No stdout")?;
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::with_capacity(CHUNK_SIZE * 2, stdout);
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).await.is_err() {
                            debug!("pipeline_consumer_dropped");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "pipeline_read_error");
                        break;
                    }
                }
            }
            debug!("pipeline_stream_complete");
        });

        self.child = Some(child);
        Ok(())
    }

    pub async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            debug!("pipeline_stopped");
        }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

pub fn find_ffmpeg() -> Option<String> {
    let candidates = if cfg!(target_os = "windows") {
        vec!["ffmpeg.exe", ".\\ffmpeg.exe"]
    } else {
        vec![
            "ffmpeg",
            "/usr/local/bin/ffmpeg",
            "/opt/homebrew/bin/ffmpeg",
        ]
    };

    for candidate in candidates {
        if std::process::Command::new(candidate)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_detection() {
        // Should find ffmpeg on dev machines
        let result = find_ffmpeg();
        if let Some(path) = result {
            println!("Found FFmpeg: {}", path);
        }
    }
}
