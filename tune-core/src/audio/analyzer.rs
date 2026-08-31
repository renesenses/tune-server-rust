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
                sample_rate = decoded.sample_rate,
                channels = decoded.channels,
                source_bit_depth = decoded.bit_depth,
                output_bit_depth = 16,
                "decoded_analyzer_contract"
            );
            // Analyzer consumers parse two bytes per sample. Returning native
            // 24/32-bit bytes under an i16 contract shifted frame boundaries
            // and corrupted BPM/waveform measurements (#2230).
            let bytes =
                super::decode::convert_pcm_bit_depth(&decoded.samples_i32, decoded.bit_depth, 16);
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

/// Streaming EBU R128 (BS.1770-4) integrated-loudness + sample-peak accumulator.
///
/// Feed interleaved, normalized (`[-1, 1]`) f64 samples in any chunking — the
/// K-weighting filter state is continuous across `feed` calls, so feeding a
/// signal all at once or in chunks yields the **same** value as the previous
/// whole-track code. Memory is bounded by one 400 ms block regardless of track
/// length: only one `f64` per 100 ms block (`block_powers`) is retained. This is
/// what lets `measure_loudness_and_peak` analyse multi-GB hi-res tracks without
/// materialising them in RAM (fixes the OOM crash-loop, #1109).
struct LoudnessAccumulator {
    channels: usize,
    block_frames: usize,
    step_frames: usize,
    /// Per-channel K-weighting biquads (state carried across `feed`).
    filters: Vec<(Biquad, Biquad)>,
    /// Per-channel K-weighted samples from the current block start onward.
    bufs: Vec<std::collections::VecDeque<f64>>,
    /// Mean-square power per 400 ms block (channel-summed).
    block_powers: Vec<f64>,
    /// Running linear sample peak on the *un-weighted* samples.
    peak: f64,
    /// Running linear TRUE peak (inter-sample, 4×) on the un-weighted
    /// samples — Catmull-Rom interpolation, see [`Self::true_peak_feed`].
    true_peak: f64,
    /// Per-channel history of the last 3 raw samples, so the interpolation
    /// stays continuous across `feed` calls (same guarantee as the
    /// K-weighting filter state).
    tp_hist: Vec<[f64; 3]>,
    total_frames: usize,
}

impl LoudnessAccumulator {
    fn new(sample_rate: usize, channels: usize) -> Self {
        let fs = sample_rate as f64;
        Self {
            channels,
            block_frames: (fs * 0.4) as usize,
            step_frames: (fs * 0.1) as usize,
            filters: (0..channels)
                .map(|_| k_weighting_coefficients(fs))
                .collect(),
            bufs: (0..channels)
                .map(|_| std::collections::VecDeque::new())
                .collect(),
            block_powers: Vec::new(),
            peak: 0.0,
            true_peak: 0.0,
            tp_hist: vec![[0.0; 3]; channels],
            total_frames: 0,
        }
    }

    /// True peak inter-échantillons (#1694) : suréchantillonnage 4× par
    /// interpolation Catmull-Rom, évaluée entre les deux derniers
    /// échantillons du canal. Assez proche de l'interpolateur BS.1770-4 pour
    /// l'usage (plafond `prevent_clipping`), et peu coûteux dans
    /// l'accumulateur déjà streaming — pas de FIR polyphase ni de tampon.
    ///
    /// L'histoire par canal traverse les appels `feed`, donc le résultat est
    /// invariant au découpage, comme le reste de l'accumulateur. Les 3
    /// zéros initiaux équivalent à un amorçage sur du silence : aucun over ne
    /// peut s'y inventer.
    fn true_peak_feed(&mut self, c: usize, raw: f64) {
        let [p0, p1, p2] = self.tp_hist[c];
        let p3 = raw;
        // Catmull-Rom entre p1 et p2, évalué en t = 1/4, 1/2, 3/4.
        let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
        let b = 2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3;
        let cc = p2 - p0;
        let d = 2.0 * p1;
        for t in [0.25f64, 0.5, 0.75] {
            let v = 0.5 * (((a * t + b) * t + cc) * t + d);
            self.true_peak = self.true_peak.max(v.abs());
        }
        self.true_peak = self.true_peak.max(raw.abs());
        self.tp_hist[c] = [p1, p2, p3];
    }

    /// Feed interleaved normalized samples. Emits every complete 400 ms block
    /// aligned on the 100 ms step (identical alignment to the batch loop
    /// `while start + block_frames <= num_frames { start += step_frames }`).
    fn feed(&mut self, interleaved: &[f64]) {
        if self.channels == 0 {
            return;
        }
        let frames = interleaved.len() / self.channels;
        for f in 0..frames {
            for c in 0..self.channels {
                let raw = interleaved[f * self.channels + c];
                self.peak = self.peak.max(raw.abs());
                self.true_peak_feed(c, raw);
                let (s1, s2) = &mut self.filters[c];
                self.bufs[c].push_back(s2.process(s1.process(raw)));
            }
            self.total_frames += 1;
        }
        if self.block_frames == 0 || self.step_frames == 0 {
            return;
        }
        while self.bufs[0].len() >= self.block_frames {
            let mut power_sum = 0.0;
            for c in 0..self.channels {
                let ms: f64 = self.bufs[c]
                    .iter()
                    .take(self.block_frames)
                    .map(|s| s * s)
                    .sum::<f64>()
                    / self.block_frames as f64;
                power_sum += ms; // channel weight = 1.0 (mono/stereo)
            }
            self.block_powers.push(power_sum);
            for c in 0..self.channels {
                self.bufs[c].drain(..self.step_frames);
            }
        }
    }

    /// Integrated loudness (LUFS, rounded to 0.1) + sample peak (clamped to
    /// 1.0) + TRUE peak (4×, volontairement NON borné à 1.0 : les overs
    /// inter-échantillons au-dessus de 0 dBFS sont précisément l'information
    /// que `prevent_clipping` doit voir, #1694). `None` for silence /
    /// below-threshold / empty input.
    fn finish(self) -> Option<(f64, f64, f64)> {
        let peak = self.peak.min(1.0);
        // Le vrai pic englobe le sample peak par construction (chaque
        // échantillon brut y participe) ; on le republie tel quel.
        let true_peak = self.true_peak;

        // Too short for even one 400 ms block: simple loudness over all samples
        // (nothing was drained, so the buffers still hold the whole signal).
        if self.block_powers.is_empty() {
            if self.total_frames == 0 {
                return None;
            }
            let mut power_sum = 0.0;
            for buf in &self.bufs {
                if buf.is_empty() {
                    return None;
                }
                power_sum += buf.iter().map(|s| s * s).sum::<f64>() / buf.len() as f64;
            }
            if power_sum <= 0.0 {
                return None;
            }
            let lufs = -0.691 + 10.0 * power_sum.log10();
            return Some(((lufs * 10.0).round() / 10.0, peak, true_peak));
        }

        // Absolute gating: keep blocks above -70 LUFS.
        let abs_threshold = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let gated_abs: Vec<f64> = self
            .block_powers
            .iter()
            .copied()
            .filter(|&p| p > abs_threshold)
            .collect();
        if gated_abs.is_empty() {
            return None;
        }
        // Relative threshold = mean of abs-gated blocks - 10 dB.
        let mean_abs: f64 = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
        let rel_threshold = mean_abs * 10.0_f64.powf(-10.0 / 10.0);
        let gated_rel: Vec<f64> = self
            .block_powers
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
        Some(((lufs * 10.0).round() / 10.0, peak, true_peak))
    }
}

/// i32 → normalized f64 scale for a given bit depth.
fn pcm_scale(bit_depth: u16) -> f64 {
    match bit_depth {
        24 => (1i64 << 23) as f64,
        32 => (1i64 << 31) as f64,
        _ => 32768.0,
    }
}

/// Integrated loudness (LUFS) from already-normalized interleaved samples.
///
/// Enveloppe d'un seul appel autour de [`LoudnessAccumulator`], utilisée par le
/// seul test d'équivalence « une passe = par morceaux » : le chemin fichier
/// pilote l'accumulateur lui-même, par segments bornés (#1109). Portée `test`
/// pour le dire — hors test, plus personne n'appelle par ici.
#[cfg(test)]
fn integrated_loudness_from_samples(
    samples: &[f64],
    sample_rate: usize,
    channels: usize,
) -> Option<f64> {
    let mut acc = LoudnessAccumulator::new(sample_rate, channels);
    acc.feed(samples);
    acc.finish().map(|(lufs, _, _)| lufs)
}

/// Measure EBU R128 integrated loudness (in LUFS) using native decoding.
///
/// Implements ITU-R BS.1770-4:
/// 1. K-frequency weighting (2-stage biquad)
/// 2. Mean-square per 400ms blocks (75% overlap)
/// 3. Absolute gating at -70 LUFS
/// 4. Relative gating at mean - 10 dB
pub async fn measure_loudness(file_path: &str) -> Option<f64> {
    measure_loudness_and_peak(file_path)
        .await
        .map(|(lufs, _, _)| lufs)
}

/// Measure the EBU R128 integrated loudness (LUFS), the linear sample peak
/// (0.0–1.0) and the linear TRUE peak (4× inter-sample, may exceed 1.0) in a
/// SINGLE decode pass — used by the ReplayGain analysis to derive
/// `rg_track_gain` (reference − LUFS), `rg_track_peak` and
/// `rg_track_true_peak` (#1694) without decoding the file twice.
pub async fn measure_loudness_and_peak(file_path: &str) -> Option<(f64, f64, f64)> {
    // Analyse in bounded time segments and stream them through the accumulator,
    // so memory never scales with track length. Decoding a whole long 24/192
    // track into RAM cost several GB and OOM-killed the server in a crash-loop
    // (#1109). K-weighting state is continuous across segments, so the result is
    // identical to a single whole-track pass. We decode at the native rate,
    // stereo (the native decoder does not resample), same as before.
    const SEG_SECONDS: f64 = 30.0;
    // Defense in depth (#1277): even if a decoder ever ignores a failed seek and
    // keeps returning the head of the track, no analysis loop may run forever.
    // 24h is far beyond any real track, so this never truncates legitimate input
    // — it only fires on a non-progressing decoder.
    const MAX_ANALYSIS_SECONDS: f64 = 24.0 * 3600.0;

    let mut acc: Option<LoudnessAccumulator> = None;
    let mut seek = 0.0_f64;

    loop {
        if seek > MAX_ANALYSIS_SECONDS {
            warn!(file = file_path, seek, "loudness_analysis_seek_cap_hit");
            break;
        }
        let path = file_path.to_string();
        let decoded = tokio::task::spawn_blocking(move || {
            super::decode::decode_to_pcm(&path, None, Some(2), seek, SEG_SECONDS)
        })
        .await
        .ok()?
        .ok()?;

        let sample_rate = decoded.sample_rate as usize;
        let channels = decoded.channels as usize;
        if sample_rate == 0 || channels == 0 || decoded.samples_i32.is_empty() {
            break; // EOF (or unreadable): done.
        }

        let scale = pcm_scale(decoded.bit_depth);
        let samples: Vec<f64> = decoded
            .samples_i32
            .iter()
            .map(|&s| s as f64 / scale)
            .collect();
        acc.get_or_insert_with(|| LoudnessAccumulator::new(sample_rate, channels))
            .feed(&samples);

        // A segment shorter than requested means we reached the end. Advance the
        // seek by the actual decoded duration so segments stay contiguous even if
        // the decoder rounds the boundary.
        let frames = decoded.samples_i32.len() / channels;
        if (frames as f64) < SEG_SECONDS * sample_rate as f64 {
            break;
        }
        seek += frames as f64 / sample_rate as f64;
    }

    acc?.finish()
}

// ---------------------------------------------------------------------------
// Trailing silence detection (pure Rust)
// ---------------------------------------------------------------------------

/// Detect trailing silence duration in seconds.
///
/// Scans backwards from the end of the file to find the last sample whose
/// absolute amplitude exceeds `threshold_db` (a negative dB value, e.g. -50).
pub async fn detect_trailing_silence(file_path: &str, threshold_db: f64) -> f64 {
    // Streamed in segments so a long track is never decoded into RAM at once
    // (same OOM class as the loudness pass, #1109). Forward scan tracking the
    // index of the last sample above threshold — equivalent to the old backward
    // scan over the whole buffer.
    const SEG_SECONDS: f64 = 30.0;
    // Defense in depth (#1277): mirror the loudness pass — never let a
    // non-progressing decoder spin this loop forever. See measure_loudness_and_peak.
    const MAX_ANALYSIS_SECONDS: f64 = 24.0 * 3600.0;
    let threshold_linear = 10.0_f64.powf(threshold_db / 20.0);

    let mut sample_rate = 0.0_f64;
    let mut total: usize = 0;
    let mut last_loud: Option<usize> = None;
    let mut seek = 0.0_f64;

    loop {
        if seek > MAX_ANALYSIS_SECONDS {
            warn!(file = file_path, seek, "trailing_silence_seek_cap_hit");
            break;
        }
        let path = file_path.to_string();
        let decoded = match tokio::task::spawn_blocking(move || {
            super::decode::decode_to_pcm(&path, None, Some(1), seek, SEG_SECONDS)
        })
        .await
        {
            Ok(Ok(d)) => d,
            _ => break,
        };

        let sr = decoded.sample_rate as usize;
        if sr == 0 || decoded.samples_i32.is_empty() {
            break;
        }
        sample_rate = sr as f64;
        // Le contrat #2230 rend normalement du mono. On compte néanmoins des
        // trames à partir des métadonnées réellement rendues : une régression
        // de l'adaptation ne doit jamais redoubler silencieusement la durée.
        let canaux = decoded.channels.max(1) as usize;
        let scale = pcm_scale(decoded.bit_depth);
        for (trame, bloc) in decoded.samples_i32.chunks(canaux).enumerate() {
            // Une trame est sonore dès qu'UN de ses canaux l'est : un silence
            // sur le seul canal gauche n'est pas un silence.
            if bloc
                .iter()
                .any(|&s| (s as f64 / scale).abs() > threshold_linear)
            {
                last_loud = Some(total + trame);
            }
        }
        let frames = decoded.samples_i32.len() / canaux;
        total += frames;
        if (frames as f64) < SEG_SECONDS * sr as f64 {
            break;
        }
        seek += frames as f64 / sr as f64;
    }

    if sample_rate <= 0.0 || total == 0 {
        return 0.0;
    }
    match last_loud {
        Some(pos) => (total - 1 - pos) as f64 / sample_rate,
        None => total as f64 / sample_rate, // entire file is silent
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

    /// The streaming `LoudnessAccumulator` must (a) match the reference batch math
    /// exactly when fed in one shot, and (b) be invariant to chunking — the two
    /// guarantees that let the file path stream in bounded memory (#1109) without
    /// changing any ReplayGain value.
    #[test]
    fn accumulator_matches_reference_and_is_chunk_invariant() {
        let sr = 48_000usize;
        let n = sr * 3; // 3 s → many 400 ms / 100 ms blocks
        let mut i16s: Vec<i16> = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            // Loud for 2 s then quiet, to exercise absolute + relative gating.
            let a = if t < 2.0 { 0.5 } else { 0.02 };
            let v = (a * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32767.0) as i16;
            i16s.push(v); // L
            i16s.push(v); // R
        }
        let f64s: Vec<f64> = i16s.iter().map(|&s| s as f64 / 32768.0).collect();

        let reference = compute_loudness_from_samples(&i16s, sr, 2).unwrap();

        let one_shot = integrated_loudness_from_samples(&f64s, sr, 2).unwrap();
        assert!(
            (one_shot - reference).abs() < 1e-9,
            "one-shot {one_shot} != reference {reference}"
        );

        // Feed in odd, frame-aligned chunks that cross block/step boundaries.
        let mut acc = LoudnessAccumulator::new(sr, 2);
        for chunk in f64s.chunks(777 * 2) {
            acc.feed(chunk);
        }
        let (chunked, _, _) = acc.finish().unwrap();
        assert!(
            (chunked - reference).abs() < 1e-9,
            "chunked {chunked} != reference {reference}"
        );
    }

    // -----------------------------------------------------------------------
    // #1694 — true peak inter-échantillons (4×, Catmull-Rom)
    // -----------------------------------------------------------------------

    /// Le cas d'école de l'over inter-échantillons : une sinusoïde à fs/4
    /// déphasée de π/4 n'est échantillonnée QUE sur ±0,707 alors que le
    /// signal continu culmine à 1,0. Le sample peak la sous-estime de 3 dB ;
    /// le true peak 4× doit voir l'essentiel de la crête manquée
    /// (Catmull-Rom en retrouve ~0,88 — pas un sinc, et c'est assumé :
    /// l'usage est le plafond `prevent_clipping`, pas la métrologie).
    #[test]
    fn true_peak_sees_the_inter_sample_over_that_sample_peak_misses() {
        let sr = 48_000usize;
        let n = sr; // 1 s
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (std::f64::consts::PI / 4.0
                + 2.0 * std::f64::consts::PI * (sr as f64 / 4.0) * i as f64 / sr as f64)
                .sin();
            samples.push(v); // L
            samples.push(v); // R
        }
        let mut acc = LoudnessAccumulator::new(sr, 2);
        acc.feed(&samples);
        let (_, peak, true_peak) = acc.finish().unwrap();

        // 1/√2 : c'est EXACTEMENT ce que le test veut dire — une sinusoïde à
        // fs/4 déphasée de π/4 n'est échantillonnée que sur ±1/√2, soit
        // −3 dB sous le vrai maximum du signal continu.
        assert!(
            (peak - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "sample peak {peak}"
        );
        assert!(
            true_peak > 0.85,
            "le true peak doit dépasser nettement le sample peak : {true_peak}"
        );
        assert!(
            true_peak >= peak,
            "le true peak englobe le sample peak par construction"
        );
    }

    /// L'histoire d'interpolation traverse les appels `feed` : nourrir le
    /// même signal en morceaux impairs doit rendre EXACTEMENT le même true
    /// peak qu'en un seul passage — même garantie que la sonie (#1109).
    #[test]
    fn true_peak_is_chunk_invariant() {
        let sr = 48_000usize;
        let n = sr / 2;
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = 0.9 * (2.0 * std::f64::consts::PI * 11_987.0 * t).sin();
            samples.push(v);
            samples.push(v * 0.5);
        }
        let mut one = LoudnessAccumulator::new(sr, 2);
        one.feed(&samples);
        let (_, _, tp_one) = one.finish().unwrap();

        let mut chunked = LoudnessAccumulator::new(sr, 2);
        for chunk in samples.chunks(101 * 2) {
            chunked.feed(chunk);
        }
        let (_, _, tp_chunked) = chunked.finish().unwrap();

        assert!(
            (tp_one - tp_chunked).abs() < 1e-12,
            "one-shot {tp_one} != chunked {tp_chunked}"
        );
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

    #[tokio::test]
    async fn decode_pcm_rend_toujours_des_trames_i16() {
        let wav_file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let path = wav_file.path().to_path_buf();
        let source = [0x7f_ffffi32, -0x80_0000i32, 0x12_3456i32, -0x12_3456i32];
        let mut data = Vec::new();
        for sample in source {
            data.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&22_050u32.to_le_bytes());
        wav.extend_from_slice(&(22_050u32 * 3).to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(&path, wav).unwrap();

        let pcm = decode_pcm(path.to_str().unwrap(), 22_050, 1, 0.0, 0.0)
            .await
            .unwrap();
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();

        assert_eq!(pcm.len(), 8, "quatre trames mono i16 = huit octets");
        assert_eq!(samples, vec![32_767, -32_768, 0x1234, -0x1235]);
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
