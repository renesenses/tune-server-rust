use std::process::Stdio;

use tracing::{debug, info, warn};

pub async fn ffmpeg_pcm(
    file_path: &str,
    sample_rate: u32,
    channels: u32,
    seek_s: f64,
    duration_s: f64,
) -> Result<Vec<u8>, String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];
    if seek_s > 0.0 {
        args.push("-ss".into());
        args.push(seek_s.to_string());
    }
    args.push("-i".into());
    args.push(file_path.to_string());
    if duration_s > 0.0 {
        args.push("-t".into());
        args.push(duration_s.to_string());
    }
    args.extend([
        "-ac".into(),
        channels.to_string(),
        "-ar".into(),
        sample_rate.to_string(),
        "-f".into(),
        "s16le".into(),
        "pipe:1".into(),
    ]);

    let output = tokio::process::Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("ffmpeg: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(file = file_path, error = %stderr, "ffmpeg_pcm_error");
        return Err(format!(
            "ffmpeg failed: {}",
            &stderr[..stderr.len().min(200)]
        ));
    }

    Ok(output.stdout)
}

pub async fn get_duration(file_path: &str) -> Result<f64, String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            file_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("ffprobe: {e}"))?;

    let s = String::from_utf8_lossy(&output.stdout);
    s.trim()
        .parse::<f64>()
        .map_err(|_| "no duration from ffprobe".into())
}

pub async fn measure_loudness(file_path: &str) -> Option<f64> {
    let output = tokio::process::Command::new("ffmpeg")
        .args([
            "-i",
            file_path,
            "-af",
            "ebur128=peak=true",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        if line.contains("I:") && line.contains("LUFS") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for (i, p) in parts.iter().enumerate() {
                if *p == "I:"
                    && let Some(val) = parts.get(i + 1)
                {
                    return val.parse::<f64>().ok();
                }
            }
        }
    }
    None
}

pub async fn detect_trailing_silence(file_path: &str, threshold_db: f64) -> f64 {
    let filter = format!("silencedetect=noise={threshold_db}dB:d=0.5");
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-i", file_path, "-af", &filter, "-f", "null", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(_) => return 0.0,
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut last_duration = 0.0_f64;
    for line in stderr.lines() {
        if line.contains("silence_end")
            && let Some(dur_part) = line.split("silence_duration:").nth(1)
            && let Ok(d) = dur_part.trim().parse::<f64>()
        {
            last_duration = d;
        }
    }
    last_duration
}

pub async fn detect_bpm(file_path: &str) -> Option<f64> {
    let sample_rate: u32 = 22050;
    let duration = 30;

    let file_duration = get_duration(file_path).await.ok()?;
    if file_duration <= 0.0 {
        return None;
    }

    let start = (file_duration / 2.0 - duration as f64 / 2.0).max(0.0);
    let pcm = ffmpeg_pcm(file_path, sample_rate, 1, start, duration as f64)
        .await
        .ok()?;

    if pcm.len() < (sample_rate as usize * 2 * 2) {
        warn!(file = file_path, "bpm_too_short");
        return None;
    }

    let samples: Vec<f64> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64)
        .collect();

    // Energy envelope via moving average
    let window = 2048_usize;
    let mut envelope: Vec<f64> = samples.iter().map(|s| s.abs()).collect();
    let mut running_sum: f64 = envelope[..window.min(envelope.len())].iter().sum();
    let len = envelope.len();
    let mut smoothed = vec![0.0_f64; len];
    for i in 0..len {
        smoothed[i] = running_sum / window as f64;
        if i + window < len {
            running_sum += envelope[i + window];
        }
        if i >= window {
            running_sum -= envelope[i - window];
        }
    }
    envelope = smoothed;

    // Remove DC offset
    let mean: f64 = envelope.iter().sum::<f64>() / envelope.len() as f64;
    for v in &mut envelope {
        *v -= mean;
    }

    // Autocorrelation for BPM range 60-200
    let min_lag = (60 * sample_rate as usize) / 200; // 200 BPM
    let max_lag = ((60 * sample_rate as usize) / 60).min(envelope.len() - 1); // 60 BPM
    if min_lag >= max_lag {
        return None;
    }

    let mut best_lag = min_lag;
    let mut best_corr = f64::NEG_INFINITY;
    for lag in min_lag..max_lag {
        let mut corr = 0.0_f64;
        let count = envelope.len() - lag;
        for i in 0..count {
            corr += envelope[i] * envelope[i + lag];
        }
        if corr > best_corr {
            best_corr = corr;
            best_lag = lag;
        }
    }

    let bpm = (60.0 * sample_rate as f64 / best_lag as f64).round();
    if !(40.0..=220.0).contains(&bpm) {
        debug!(file = file_path, bpm, "bpm_out_of_range");
        return None;
    }

    info!(file = file_path, bpm, "bpm_detected");
    Some(bpm)
}

pub async fn generate_waveform(file_path: &str, points: usize) -> Vec<f32> {
    let sample_rate = 22050_u32;

    let pcm = match ffmpeg_pcm(file_path, sample_rate, 1, 0.0, 0.0).await {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let samples: Vec<f64> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64)
        .collect();

    if samples.len() < points {
        return Vec::new();
    }

    let frame_size = samples.len() / points;
    let mut rms_values: Vec<f64> = (0..points)
        .map(|i| {
            let start = i * frame_size;
            let end = start + frame_size;
            let frame = &samples[start..end];
            let mean_sq = frame.iter().map(|s| s * s).sum::<f64>() / frame.len() as f64;
            mean_sq.sqrt()
        })
        .collect();

    let max_rms = rms_values.iter().cloned().fold(0.0_f64, f64::max);
    if max_rms > 0.0 {
        for v in &mut rms_values {
            *v /= max_rms;
        }
    }

    rms_values
        .iter()
        .map(|v| (*v as f32 * 10000.0).round() / 10000.0)
        .collect()
}

pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_check() {
        let available = ffmpeg_available();
        if available {
            println!("ffmpeg found");
        } else {
            println!("ffmpeg not found (some features disabled)");
        }
    }

    #[test]
    fn waveform_normalize() {
        let rms = vec![0.5_f64, 1.0, 0.25];
        let max = rms.iter().cloned().fold(0.0_f64, f64::max);
        let normalized: Vec<f32> = rms.iter().map(|v| (v / max) as f32).collect();
        assert!((normalized[0] - 0.5).abs() < 0.01);
        assert!((normalized[1] - 1.0).abs() < 0.01);
        assert!((normalized[2] - 0.25).abs() < 0.01);
    }

    #[test]
    fn bpm_range_validation() {
        assert!((40.0..=220.0).contains(&120.0));
        assert!(!(40.0..=220.0).contains(&300.0));
        assert!(!(40.0..=220.0).contains(&10.0));
    }

    #[test]
    fn pcm_format_parse() {
        let bytes: [u8; 4] = [0x00, 0x40, 0x00, 0xC0]; // 16384, -16384
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![16384, -16384]);
    }

    #[test]
    fn moving_average_smoothing() {
        let data = vec![0.0, 0.0, 10.0, 0.0, 0.0];
        let window = 3_usize;
        let smoothed: Vec<f64> = (0..data.len())
            .map(|i| {
                let start = i.saturating_sub(window / 2);
                let end = (i + window / 2 + 1).min(data.len());
                let slice = &data[start..end];
                slice.iter().sum::<f64>() / slice.len() as f64
            })
            .collect();
        assert!(smoothed[2] < 10.0);
        assert!(smoothed[2] > 0.0);
    }
}
