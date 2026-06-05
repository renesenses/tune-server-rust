use std::process::Stdio;

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
    started: bool,
    tx: mpsc::Sender<Vec<u8>>,
    config: PipelineConfig,
}

impl AudioPipeline {
    pub fn new(config: PipelineConfig, buffer_size: usize) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            Self {
                started: false,
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
        let cfg = self.config.clone();
        let tx = self.tx.clone();

        info!(file = %cfg.file_path, format = ?cfg.output_format, "pipeline_start_native");

        tokio::spawn(async move {
            if let Err(e) = run_native_pipeline(&cfg, &tx).await {
                warn!(error = %e, file = %cfg.file_path, "pipeline_native_error");
            }
            debug!("pipeline_stream_complete");
        });

        self.started = true;
        Ok(())
    }

    pub async fn stop(&mut self) {
        self.started = false;
        debug!("pipeline_stopped");
    }

    pub fn is_running(&self) -> bool {
        self.started
    }
}

/// Decode the file natively, optionally encode to the target format, and send
/// chunks through the channel.
async fn run_native_pipeline(
    cfg: &PipelineConfig,
    tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), String> {
    let seek_s = cfg.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);

    // Decode to PCM (i16 samples, interleaved)
    let decoded = super::decode::decode_to_pcm(
        &cfg.file_path,
        Some(cfg.sample_rate),
        Some(cfg.channels as u32),
        seek_s,
        0.0, // no duration limit
    )
    .map_err(|e| format!("native decode failed: {e}"))?;

    // Convert i16 samples to raw PCM bytes (16-bit LE)
    let pcm_bytes: Vec<u8> = decoded
        .samples
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();

    // Encode to the target format
    let output_data = match cfg.output_format {
        AudioFormat::Wav | AudioFormat::Flac => {
            let mut encoder = super::encoder::AudioEncoder::new(
                cfg.output_format.ffmpeg_format_arg(),
                decoded.sample_rate,
                16, // decode_to_pcm produces i16
                decoded.channels,
            );
            encoder.start().await?;
            encoder.write(&pcm_bytes).await?;
            encoder.finish().await?
        }
        _ => {
            // For other formats, encode as FLAC (the encoder handles fallback)
            let mut encoder = super::encoder::AudioEncoder::new(
                cfg.output_format.ffmpeg_format_arg(),
                decoded.sample_rate,
                16,
                decoded.channels,
            );
            encoder.start().await?;
            encoder.write(&pcm_bytes).await?;
            encoder.finish().await?
        }
    };

    // Send in chunks
    for chunk in output_data.chunks(CHUNK_SIZE) {
        if tx.send(chunk.to_vec()).await.is_err() {
            debug!("pipeline_consumer_dropped");
            return Ok(());
        }
    }

    Ok(())
}

pub fn find_ffmpeg() -> Option<String> {
    // 1. Check bundled / well-known absolute paths first
    let bundled: Vec<&str> = if cfg!(target_os = "windows") {
        vec![".\\ffmpeg.exe"]
    } else {
        vec!["/usr/local/bin/ffmpeg", "/opt/homebrew/bin/ffmpeg"]
    };

    for candidate in &bundled {
        if std::process::Command::new(candidate)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            info!(path = %candidate, "ffmpeg_found_bundled");
            return Some(candidate.to_string());
        }
    }

    // 2. Fall back to PATH lookup (bare "ffmpeg" — the OS resolves via PATH)
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        info!("ffmpeg_found_in_path");
        return Some("ffmpeg".to_string());
    }

    warn!("ffmpeg_not_found");
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

    #[tokio::test]
    async fn pipeline_stream_info() {
        let config = PipelineConfig {
            file_path: "test.flac".into(),
            output_format: AudioFormat::Wav,
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            seek_ms: None,
        };
        let (pipeline, _rx) = AudioPipeline::new(config, 16);
        let info = pipeline.stream_info();
        assert_eq!(info.format, AudioFormat::Wav);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.bit_depth, 16);
        assert_eq!(info.channels, 2);
        assert_eq!(info.mime_type, "audio/wav");
    }

    #[tokio::test]
    async fn native_pipeline_wav_fixture() {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/fixtures/test.wav");
        if !path.exists() {
            return; // skip if fixture missing
        }

        let config = PipelineConfig {
            file_path: path.to_string_lossy().to_string(),
            output_format: AudioFormat::Wav,
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            seek_ms: None,
        };
        let (mut pipeline, mut rx) = AudioPipeline::new(config, 64);
        pipeline.start().await.unwrap();

        // Drop the pipeline so self.tx is released and the channel closes
        // after the spawned decode task completes.
        drop(pipeline);

        let mut total_bytes = 0;
        while let Some(chunk) = rx.recv().await {
            total_bytes += chunk.len();
        }
        assert!(
            total_bytes > 44,
            "should produce WAV output with header + data"
        );
    }
}
