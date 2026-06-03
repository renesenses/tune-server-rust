use std::io::Cursor;
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
    wav_buffer: Option<Vec<u8>>,
}

impl FFmpegEncoder {
    pub fn new(format: &str, sample_rate: u32, bit_depth: u32, channels: u32) -> Self {
        Self {
            format: format.to_string(),
            sample_rate,
            bit_depth,
            channels,
            process: None,
            wav_buffer: None,
        }
    }

    pub async fn start(&mut self) -> Result<(), String> {
        if self.format == "wav" {
            debug!(
                format = "wav",
                sample_rate = self.sample_rate,
                bit_depth = self.bit_depth,
                "encoder_start_hound"
            );
            self.wav_buffer = Some(Vec::new());
            return Ok(());
        }

        // FFmpeg for non-WAV formats (FLAC, MP3, OGG, etc.)
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
            "flac" => args.extend(["-f".into(), "flac".into()]),
            "mp3" => args.extend([
                "-codec:a".into(),
                "libmp3lame".into(),
                "-q:a".into(),
                "2".into(),
                "-f".into(),
                "mp3".into(),
            ]),
            "ogg" => args.extend([
                "-codec:a".into(),
                "libvorbis".into(),
                "-q:a".into(),
                "6".into(),
                "-f".into(),
                "ogg".into(),
            ]),
            other => args.extend(["-f".into(), other.to_string()]),
        }
        args.push("pipe:1".into());

        debug!(
            format = self.format,
            sample_rate = self.sample_rate,
            "encoder_start_ffmpeg"
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
        if let Some(ref mut buf) = self.wav_buffer {
            buf.extend_from_slice(pcm_data);
            return Ok(());
        }

        let child = self.process.as_mut().ok_or("encoder not started")?;
        let stdin = child.stdin.as_mut().ok_or("no stdin")?;
        stdin
            .write_all(pcm_data)
            .await
            .map_err(|e| format!("encoder write: {e}"))?;
        Ok(())
    }

    pub async fn finish(&mut self) -> Result<Vec<u8>, String> {
        if let Some(pcm_data) = self.wav_buffer.take() {
            return encode_wav_hound(&pcm_data, self.sample_rate, self.bit_depth, self.channels);
        }

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
        self.wav_buffer = None;
    }
}

fn encode_wav_hound(
    pcm: &[u8],
    sample_rate: u32,
    bit_depth: u32,
    channels: u32,
) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate,
        bits_per_sample: bit_depth as u16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    let mut writer =
        hound::WavWriter::new(&mut cursor, spec).map_err(|e| format!("hound init: {e}"))?;

    match bit_depth {
        16 => {
            for chunk in pcm.chunks_exact(2) {
                let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        24 => {
            for chunk in pcm.chunks_exact(3) {
                let sample = i32::from_le_bytes([
                    chunk[0],
                    chunk[1],
                    chunk[2],
                    if chunk[2] & 0x80 != 0 { 0xFF } else { 0 },
                ]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        32 => {
            for chunk in pcm.chunks_exact(4) {
                let sample = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                writer
                    .write_sample(sample)
                    .map_err(|e| format!("hound write: {e}"))?;
            }
        }
        _ => return Err(format!("unsupported bit depth: {bit_depth}")),
    }

    writer
        .finalize()
        .map_err(|e| format!("hound finalize: {e}"))?;
    debug!(
        pcm_bytes = pcm.len(),
        wav_bytes = cursor.get_ref().len(),
        "wav_encoded_hound"
    );
    Ok(cursor.into_inner())
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

    #[tokio::test]
    async fn wav_encode_hound() {
        let mut enc = FFmpegEncoder::new("wav", 44100, 16, 2);
        enc.start().await.unwrap();
        // 100 frames of silence (stereo 16-bit = 400 bytes)
        let pcm = vec![0u8; 400];
        enc.write(&pcm).await.unwrap();
        let wav = enc.finish().await.unwrap();
        // WAV has 44-byte header
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn wav_encode_24bit() {
        let pcm = vec![0u8; 600]; // 100 frames * 2ch * 3 bytes
        let wav = encode_wav_hound(&pcm, 96000, 24, 2).unwrap();
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
    }
}
