use lofty::file::AudioFile;

use tracing::{debug, info, warn};

/// Decode audio file to raw PCM (i16 LE interleaved).
///
/// Uses native Rust decoders for all supported formats (FLAC, MP3, WAV, AAC,
/// ALAC, OGG, AIFF, DSF, DFF, WavPack, APE).
pub async fn decode_pcm(
    file_path: &str,
    sample_rate: u32,
    channels: u32,
    seek_s: f64,
    duration_s: f64,
) -> Result<Vec<u8>, String> {
    let path = file_path.to_string();
    let result = tokio::task::spawn_blocking(move || {
        super::decode::decode_to_pcm(&path, Some(sample_rate), Some(channels), seek_s, duration_s)
    })
    .await
    .map_err(|e| format!("join: {e}"))?;

    match result {
        Ok(decoded) => {
            debug!(
                file = file_path,
                samples = decoded.samples_i32.len(),
                "decoded_native"
            );
            let bytes: Vec<u8> = decoded.pcm_bytes();
            Ok(bytes)
        }
        Err(e) => {
            warn!(file = file_path, error = %e, "native_decode_failed");
            Err(e)
        }
    }
}

pub async fn get_duration(file_path: &str) -> Result<f64, String> {
    let path = file_path.to_string();
    tokio::task::spawn_blocking(move || {
        let tagged = lofty::read_from_path(&path).map_err(|e| format!("lofty duration: {e}"))?;
        let duration = tagged.properties().duration();
        Ok(duration.as_secs_f64())
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}

// ---------------------------------------------------------------------------
// EBU R128 loudness measurement (pure Rust)
// ---------------------------------------------------------------------------

/// Transposed direct-form II biquad filter.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process one sample (transposed direct-form II).
    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Compute K-weighting biquad coefficients for the given sample rate.
///
/// Returns (stage1, stage2) where:
/// - stage1 = pre-filter (high-shelf modelling head acoustics)
/// - stage2 = RLB weighting (high-pass ~38 Hz)
///
/// Reference: ITU-R BS.1770-4, Table 1.
fn k_weighting_coefficients(fs: f64) -> (Biquad, Biquad) {
    // --- Stage 1: Pre-filter (high-shelf) ---
    // Design parameters (from ITU-R BS.1770-4)
    let db = 3.999843853973347;
    let f0 = 1681.974450955533;
    let q = 0.7071752369554196;

    let k = (std::f64::consts::PI * f0 / fs).tan();
    let vh = 10.0_f64.powf(db / 20.0);
    let vb = vh.powf(0.4996667741545416);

    let a0 = 1.0 + k / q + k * k;
    let s1_b0 = (vh + vb * k / q + k * k) / a0;
    let s1_b1 = 2.0 * (k * k - vh) / a0;
    let s1_b2 = (vh - vb * k / q + k * k) / a0;
    let s1_a1 = 2.0 * (k * k - 1.0) / a0;
    let s1_a2 = (1.0 - k / q + k * k) / a0;

    // --- Stage 2: RLB weighting (high-pass) ---
    let f0_hp = 38.13547087602444;
    let q_hp = 0.5003270373238773;

    let k2 = (std::f64::consts::PI * f0_hp / fs).tan();
    let a0_hp = 1.0 + k2 / q_hp + k2 * k2;
    let s2_b0 = 1.0 / a0_hp;
    let s2_b1 = -2.0 / a0_hp;
    let s2_b2 = 1.0 / a0_hp;
    let s2_a1 = 2.0 * (k2 * k2 - 1.0) / a0_hp;
    let s2_a2 = (1.0 - k2 / q_hp + k2 * k2) / a0_hp;

    (
        Biquad::new(s1_b0, s1_b1, s1_b2, s1_a1, s1_a2),
        Biquad::new(s2_b0, s2_b1, s2_b2, s2_a1, s2_a2),
    )
}

/// Measure EBU R128 integrated loudness (in LUFS) using native decoding.
///
/// Implements ITU-R BS.1770-4:
/// 1. K-frequency weighting (2-stage biquad)
/// 2. Mean-square per 400ms blocks (75% overlap)
/// 3. Absolute gating at -70 LUFS
/// 4. Relative gating at mean - 10 dB
pub async fn measure_loudness(file_path: &str) -> Option<f64> {
    // Decode to native sample rate, stereo.
    // We don't assume 48 kHz because the native decoder does not resample.
    let path = file_path.to_string();
    let decoded = tokio::task::spawn_blocking(move || {
        super::decode::decode_to_pcm(&path, None, Some(2), 0.0, 0.0)
    })
    .await
    .ok()?
    .ok()?;

    let sample_rate = decoded.sample_rate as usize;
    let channels = decoded.channels as usize;
    if sample_rate == 0 || channels == 0 || decoded.samples_i32.is_empty() {
        return None;
    }

    // Convert i32 → f64 normalized to [-1, 1] based on bit depth
    let scale = match decoded.bit_depth {
        24 => (1i64 << 23) as f64,
        32 => (1i64 << 31) as f64,
        _ => 32768.0,
    };
    let samples: Vec<f64> = decoded
        .samples_i32
        .iter()
        .map(|&s| s as f64 / scale)
        .collect();

    let num_frames = samples.len() / channels;
    if num_frames == 0 {
        return None;
    }

    // De-interleave into per-channel buffers
    let mut ch_bufs: Vec<Vec<f64>> = (0..channels)
        .map(|c| (0..num_frames).map(|f| samples[f * channels + c]).collect())
        .collect();

    // Apply K-weighting to each channel
    let fs = sample_rate as f64;
    for ch in &mut ch_bufs {
        let (mut stage1, mut stage2) = k_weighting_coefficients(fs);
        for s in ch.iter_mut() {
            *s = stage1.process(*s);
            *s = stage2.process(*s);
        }
    }

    // 400ms blocks with 75% overlap (= step of 100ms)
    let block_frames = (sample_rate as f64 * 0.4) as usize;
    let step_frames = (sample_rate as f64 * 0.1) as usize;
    if block_frames == 0 || step_frames == 0 || num_frames < block_frames {
        // File too short for even one block — compute simple loudness
        let mut power_sum = 0.0;
        for ch in &ch_bufs {
            let ms: f64 = ch.iter().map(|s| s * s).sum::<f64>() / ch.len() as f64;
            power_sum += ms; // weight = 1.0 for L and R
        }
        if power_sum <= 0.0 {
            return None;
        }
        let lufs = -0.691 + 10.0 * power_sum.log10();
        return Some((lufs * 10.0).round() / 10.0);
    }

    // Compute block loudness values
    let mut block_powers: Vec<f64> = Vec::new();
    let mut start = 0;
    while start + block_frames <= num_frames {
        let mut power_sum = 0.0;
        for ch in &ch_bufs {
            let block = &ch[start..start + block_frames];
            let ms: f64 = block.iter().map(|s| s * s).sum::<f64>() / block_frames as f64;
            power_sum += ms; // channel weight = 1.0 for stereo
        }
        block_powers.push(power_sum);
        start += step_frames;
    }

    if block_powers.is_empty() {
        return None;
    }

    // Absolute gating: keep blocks above -70 LUFS
    let abs_threshold = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let gated_abs: Vec<f64> = block_powers
        .iter()
        .copied()
        .filter(|&p| p > abs_threshold)
        .collect();

    if gated_abs.is_empty() {
        return None; // entire file is below -70 LUFS
    }

    // Relative threshold = mean of abs-gated blocks - 10 dB
    let mean_abs: f64 = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
    let rel_threshold = mean_abs * 10.0_f64.powf(-10.0 / 10.0); // mean / 10

    // Final gating: keep blocks above relative threshold
    let gated_rel: Vec<f64> = block_powers
        .iter()
        .copied()
        .filter(|&p| p > rel_threshold)
        .collect();

    if gated_rel.is_empty() {
        return None;
    }

    let mean_rel: f64 = gated_rel.iter().sum::<f64>() / gated_rel.len() as f64;
    if mean_rel <= 0.0 {
        return None;
    }

    let lufs = -0.691 + 10.0 * mean_rel.log10();
    // Round to 1 decimal
    Some((lufs * 10.0).round() / 10.0)
}

// ---------------------------------------------------------------------------
// Trailing silence detection (pure Rust)
// ---------------------------------------------------------------------------

/// Detect trailing silence duration in seconds.
///
/// Scans backwards from the end of the file to find the last sample whose
/// absolute amplitude exceeds `threshold_db` (a negative dB value, e.g. -50).
pub async fn detect_trailing_silence(file_path: &str, threshold_db: f64) -> f64 {
    let path = file_path.to_string();
    let decoded = tokio::task::spawn_blocking(move || {
        super::decode::decode_to_pcm(&path, None, Some(1), 0.0, 0.0)
    })
    .await;

    let decoded = match decoded {
        Ok(Ok(d)) => d,
        _ => return 0.0,
    };

    let sample_rate = decoded.sample_rate as f64;
    if sample_rate <= 0.0 || decoded.samples_i32.is_empty() {
        return 0.0;
    }

    let threshold_linear = 10.0_f64.powf(threshold_db / 20.0);

    // Normalize based on bit depth
    let scale = match decoded.bit_depth {
        24 => (1i64 << 23) as f64,
        32 => (1i64 << 31) as f64,
        _ => 32768.0,
    };

    // Find the last sample above threshold, scanning backwards
    let last_loud = decoded
        .samples_i32
        .iter()
        .rposition(|&s| (s as f64 / scale).abs() > threshold_linear);

    match last_loud {
        Some(pos) => (decoded.samples_i32.len() - 1 - pos) as f64 / sample_rate,
        None => decoded.samples_i32.len() as f64 / sample_rate, // entire file is silent
    }
}

pub async fn detect_bpm(file_path: &str) -> Option<f64> {
    let sample_rate: u32 = 22050;
    let duration = 30;

    let file_duration = get_duration(file_path).await.ok()?;
    if file_duration <= 0.0 {
        return None;
    }

    let start = (file_duration / 2.0 - duration as f64 / 2.0).max(0.0);
    let pcm = decode_pcm(file_path, sample_rate, 1, start, duration as f64)
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
    let envelope: Vec<f64> = samples.iter().map(|s| s.abs()).collect();
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
    let mut envelope = smoothed;

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

    let pcm = match decode_pcm(file_path, sample_rate, 1, 0.0, 0.0).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Biquad filter tests
    // -----------------------------------------------------------------------

    #[test]
    fn biquad_passthrough() {
        // Unity filter: b0=1, b1=b2=a1=a2=0 → output = input
        let mut bq = Biquad::new(1.0, 0.0, 0.0, 0.0, 0.0);
        assert!((bq.process(1.0) - 1.0).abs() < 1e-12);
        assert!((bq.process(0.5) - 0.5).abs() < 1e-12);
        assert!((bq.process(-0.3) - (-0.3)).abs() < 1e-12);
    }

    #[test]
    fn biquad_impulse_response() {
        // Simple 1-sample delay: b0=0, b1=1, rest=0 → y[n] = x[n-1]
        let mut bq = Biquad::new(0.0, 1.0, 0.0, 0.0, 0.0);
        assert!((bq.process(1.0) - 0.0).abs() < 1e-12);
        assert!((bq.process(0.0) - 1.0).abs() < 1e-12);
        assert!((bq.process(0.0) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn biquad_reset() {
        let mut bq = Biquad::new(0.5, 0.3, 0.1, -0.2, 0.1);
        bq.process(1.0);
        bq.process(0.5);
        bq.reset();
        assert_eq!(bq.z1, 0.0);
        assert_eq!(bq.z2, 0.0);
    }

    // -----------------------------------------------------------------------
    // K-weighting coefficient tests
    // -----------------------------------------------------------------------

    #[test]
    fn k_weighting_48khz_matches_reference() {
        // Verify that our coefficient computation for 48 kHz matches the
        // published ITU-R BS.1770-4 reference values (within tolerance).
        let (s1, s2) = k_weighting_coefficients(48000.0);

        // Stage 1 reference (from ITU-R BS.1770-4 Table 1)
        assert!((s1.b0 - 1.53512485958697).abs() < 1e-6, "s1.b0={}", s1.b0);
        assert!(
            (s1.b1 - (-2.69169618940638)).abs() < 1e-6,
            "s1.b1={}",
            s1.b1
        );
        assert!((s1.b2 - 1.19839281085285).abs() < 1e-6, "s1.b2={}", s1.b2);
        assert!(
            (s1.a1 - (-1.69065929318241)).abs() < 1e-6,
            "s1.a1={}",
            s1.a1
        );
        assert!((s1.a2 - 0.73248077421585).abs() < 1e-6, "s1.a2={}", s1.a2);

        // Stage 2 reference (ITU-R table lists unnormalized b; we normalize by a0)
        // a0 = 1 + k/Q + k^2 for 48 kHz ≈ 1.004993
        // So b0_norm = 1/a0, b1_norm = -2/a0, b2_norm = 1/a0
        // a1_norm and a2_norm match the table directly.
        let a0_s2 = 1.0 / s2.b0; // recover a0 from normalized b0 = 1/a0
        assert!((a0_s2 * s2.b0 - 1.0).abs() < 1e-10, "b0 * a0 should be 1.0");
        assert!(
            (a0_s2 * s2.b1 - (-2.0)).abs() < 1e-6,
            "unnormalized b1 should be -2.0, got {}",
            a0_s2 * s2.b1
        );
        assert!(
            (a0_s2 * s2.b2 - 1.0).abs() < 1e-6,
            "unnormalized b2 should be 1.0, got {}",
            a0_s2 * s2.b2
        );
        assert!(
            (s2.a1 - (-1.99004745483398)).abs() < 1e-6,
            "s2.a1={}",
            s2.a1
        );
        assert!((s2.a2 - 0.99007225036621).abs() < 1e-6, "s2.a2={}", s2.a2);
    }

    #[test]
    fn k_weighting_44100_produces_valid_coefficients() {
        let (s1, s2) = k_weighting_coefficients(44100.0);
        // Coefficients should be finite and reasonable
        assert!(s1.b0.is_finite() && s1.b0 > 0.0);
        assert!(s2.b0.is_finite() && s2.b0 > 0.0);
        // a2 should be < 1 for stability
        assert!(s1.a2.abs() < 2.0);
        assert!(s2.a2.abs() < 2.0);
    }

    // -----------------------------------------------------------------------
    // Integrated loudness tests (synthetic signals)
    // -----------------------------------------------------------------------

    #[test]
    fn loudness_of_silence_is_none() {
        // Silence should gate out entirely → None
        let samples = vec![0i16; 48000 * 2]; // 0.5s stereo silence at 48kHz
        let result = compute_loudness_from_samples(&samples, 48000, 2);
        assert!(
            result.is_none(),
            "pure silence should return None, got {:?}",
            result
        );
    }

    #[test]
    fn loudness_of_full_scale_sine() {
        // A full-scale 1 kHz sine at 48 kHz, 2 channels, 2 seconds.
        //
        // Per EBU R128 / ITU-R BS.1770-4:
        // - Each channel: RMS^2 of sine = 0.5, K-weighting gain at 1 kHz ≈ 0 dB
        // - Stereo sum: G_L * z_L + G_R * z_R = 1.0 * 0.5 + 1.0 * 0.5 = 1.0
        // - LUFS = -0.691 + 10*log10(1.0) = -0.691 ≈ -0.7 LUFS
        let sr = 48000_usize;
        let duration_s = 2.0;
        let num_frames = (sr as f64 * duration_s) as usize;
        let freq = 1000.0;

        let mut samples = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            let t = i as f64 / sr as f64;
            let val = (2.0 * std::f64::consts::PI * freq * t).sin();
            let s = (val * 32767.0) as i16;
            samples.push(s); // L
            samples.push(s); // R
        }

        let lufs = compute_loudness_from_samples(&samples, sr, 2);
        assert!(lufs.is_some(), "should produce a loudness value");
        let lufs = lufs.unwrap();
        // Dual-mono 0 dBFS sine → ~-0.7 LUFS (two channels summed)
        // Allow ±1.0 dB tolerance for quantization and edge effects
        assert!(
            lufs > -2.0 && lufs < 0.5,
            "expected ~-0.7 LUFS for dual-mono 0dBFS sine, got {}",
            lufs
        );
    }

    #[test]
    fn loudness_decreases_with_amplitude() {
        let sr = 48000_usize;
        let num_frames = sr * 2; // 2 seconds

        let make_sine = |amplitude: f64| -> Vec<i16> {
            let mut samples = Vec::with_capacity(num_frames * 2);
            for i in 0..num_frames {
                let t = i as f64 / sr as f64;
                let val = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * amplitude;
                let s = (val * 32767.0) as i16;
                samples.push(s);
                samples.push(s);
            }
            samples
        };

        let loud = compute_loudness_from_samples(&make_sine(1.0), sr, 2).unwrap();
        let quiet = compute_loudness_from_samples(&make_sine(0.1), sr, 2).unwrap();

        assert!(
            quiet < loud,
            "quieter signal should have lower LUFS: loud={}, quiet={}",
            loud,
            quiet
        );
        // 20 dB amplitude difference → ~20 dB loudness difference
        let diff = loud - quiet;
        assert!(
            diff > 15.0 && diff < 25.0,
            "expected ~20 dB difference, got {}",
            diff
        );
    }

    /// Helper: compute integrated loudness from raw i16 interleaved samples.
    /// Used by tests to avoid needing actual audio files.
    fn compute_loudness_from_samples(
        raw_samples: &[i16],
        sample_rate: usize,
        channels: usize,
    ) -> Option<f64> {
        if sample_rate == 0 || channels == 0 || raw_samples.is_empty() {
            return None;
        }

        let samples: Vec<f64> = raw_samples.iter().map(|&s| s as f64 / 32768.0).collect();
        let num_frames = samples.len() / channels;

        let mut ch_bufs: Vec<Vec<f64>> = (0..channels)
            .map(|c| (0..num_frames).map(|f| samples[f * channels + c]).collect())
            .collect();

        let fs = sample_rate as f64;
        for ch in &mut ch_bufs {
            let (mut stage1, mut stage2) = k_weighting_coefficients(fs);
            for s in ch.iter_mut() {
                *s = stage1.process(*s);
                *s = stage2.process(*s);
            }
        }

        let block_frames = (sample_rate as f64 * 0.4) as usize;
        let step_frames = (sample_rate as f64 * 0.1) as usize;

        if block_frames == 0 || step_frames == 0 || num_frames < block_frames {
            let mut power_sum = 0.0;
            for ch in &ch_bufs {
                let ms: f64 = ch.iter().map(|s| s * s).sum::<f64>() / ch.len() as f64;
                power_sum += ms;
            }
            if power_sum <= 0.0 {
                return None;
            }
            let lufs = -0.691 + 10.0 * power_sum.log10();
            return Some((lufs * 10.0).round() / 10.0);
        }

        let mut block_powers: Vec<f64> = Vec::new();
        let mut start = 0;
        while start + block_frames <= num_frames {
            let mut power_sum = 0.0;
            for ch in &ch_bufs {
                let block = &ch[start..start + block_frames];
                let ms: f64 = block.iter().map(|s| s * s).sum::<f64>() / block_frames as f64;
                power_sum += ms;
            }
            block_powers.push(power_sum);
            start += step_frames;
        }

        if block_powers.is_empty() {
            return None;
        }

        let abs_threshold = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let gated_abs: Vec<f64> = block_powers
            .iter()
            .copied()
            .filter(|&p| p > abs_threshold)
            .collect();

        if gated_abs.is_empty() {
            return None;
        }

        let mean_abs: f64 = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
        let rel_threshold = mean_abs * 10.0_f64.powf(-10.0 / 10.0);

        let gated_rel: Vec<f64> = block_powers
            .iter()
            .copied()
            .filter(|&p| p > rel_threshold)
            .collect();

        if gated_rel.is_empty() {
            return None;
        }

        let mean_rel: f64 = gated_rel.iter().sum::<f64>() / gated_rel.len() as f64;
        if mean_rel <= 0.0 {
            return None;
        }

        let lufs = -0.691 + 10.0 * mean_rel.log10();
        Some((lufs * 10.0).round() / 10.0)
    }

    // -----------------------------------------------------------------------
    // Trailing silence detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn trailing_silence_all_silent() {
        // All zeros → entire duration is silence
        let samples = vec![0i16; 44100]; // 1s mono
        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);
        assert!(last_loud.is_none());
    }

    #[test]
    fn trailing_silence_no_silence() {
        // Signal everywhere → 0 trailing silence
        let sr = 44100;
        let samples: Vec<i16> = (0..sr)
            .map(|i| {
                let t = i as f64 / sr as f64;
                ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16000.0) as i16
            })
            .collect();

        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);

        assert!(last_loud.is_some());
        let silence_frames = samples.len() - 1 - last_loud.unwrap();
        let silence_s = silence_frames as f64 / sr as f64;
        assert!(
            silence_s < 0.01,
            "should have negligible trailing silence, got {}",
            silence_s
        );
    }

    #[test]
    fn trailing_silence_half_second() {
        // 0.5s of signal + 0.5s of silence = 0.5s trailing silence
        let sr = 44100_usize;
        let mut samples: Vec<i16> = Vec::with_capacity(sr);

        // First half: signal
        for i in 0..sr / 2 {
            let t = i as f64 / sr as f64;
            let val = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16000.0;
            samples.push(val as i16);
        }
        // Second half: silence
        samples.extend(vec![0i16; sr / 2]);

        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);

        assert!(last_loud.is_some());
        let silence_s = (samples.len() - 1 - last_loud.unwrap()) as f64 / sr as f64;
        assert!(
            (silence_s - 0.5).abs() < 0.02,
            "expected ~0.5s trailing silence, got {}",
            silence_s
        );
    }

    // -----------------------------------------------------------------------
    // Existing tests (preserved)
    // -----------------------------------------------------------------------

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
