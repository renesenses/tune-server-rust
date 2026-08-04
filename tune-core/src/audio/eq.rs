//! Parametric equalizer for the Tune Master Profiler.
//!
//! 3-band EQ using biquad filters (Robert Bristow-Johnson Audio EQ Cookbook):
//! - Low shelf (60-80 Hz) — bass resonance correction
//! - Mid peak (1-3 kHz) — voice presence/clarity
//! - High shelf (10-12 kHz) — treble air/brightness
//!
//! Processing is done in f64 for bit-perfect quality. The EQ profile is
//! stored per-zone and applied in the PCM pipeline before output.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// User-facing EQ profile combining room macro settings + perceptual sliders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqProfile {
    pub enabled: bool,
    /// Macro environment
    pub listening: ListeningMode,
    pub room_size: RoomSize,
    pub speaker_placement: SpeakerPlacement,
    /// Perceptual sliders: -12.0 to +12.0 dB
    pub bass_gain_db: f64,
    pub mid_gain_db: f64,
    pub treble_gain_db: f64,
    /// Expert-mode bands (graphic 10/15/31 or parametric). When non-empty they
    /// REPLACE the 3-tilt cascade above — the two modes are alternative UIs
    /// over the same per-zone profile. `default` keeps every profile persisted
    /// before this field deserializing unchanged.
    #[serde(default)]
    pub bands: Vec<EqBandSpec>,
}

/// One expert-mode filter band (RBJ biquad).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBandSpec {
    /// Center/corner frequency in Hz
    pub freq: f64,
    /// Gain in dB (shelves/peak; ignored by pass/notch)
    #[serde(default)]
    pub gain: f64,
    #[serde(default = "default_band_q")]
    pub q: f64,
    /// "peak" | "low_shelf" | "high_shelf" | "low_pass" | "high_pass" | "notch"
    #[serde(rename = "type", default = "default_band_type")]
    pub band_type: String,
}

fn default_band_q() -> f64 {
    1.0
}

fn default_band_type() -> String {
    "peak".into()
}

impl EqBandSpec {
    /// A band that would leave the signal untouched (flat peak/shelf).
    fn is_neutral(&self) -> bool {
        matches!(self.band_type.as_str(), "peak" | "low_shelf" | "high_shelf")
            && self.gain.abs() < 0.01
    }

    fn coeffs(&self, sample_rate: f64) -> BiquadCoeffs {
        // Clamp to sane, stable ranges (mirrors the /eq router validation).
        let freq = self.freq.clamp(10.0, sample_rate * 0.45);
        let q = self.q.clamp(0.1, 30.0);
        let gain = self.gain.clamp(-24.0, 24.0);
        match self.band_type.as_str() {
            "low_shelf" => low_shelf(freq, gain, sample_rate),
            "high_shelf" => high_shelf(freq, gain, sample_rate),
            "low_pass" => low_pass(freq, q, sample_rate),
            "high_pass" => high_pass(freq, q, sample_rate),
            "notch" => notch(freq, q, sample_rate),
            _ => peaking_eq(freq, gain, q, sample_rate),
        }
    }
}

impl Default for EqProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            listening: ListeningMode::Speakers,
            room_size: RoomSize::Medium,
            speaker_placement: SpeakerPlacement::FreeStanding,
            bass_gain_db: 0.0,
            mid_gain_db: 0.0,
            treble_gain_db: 0.0,
            bands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ListeningMode {
    Headphones,
    Speakers,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoomSize {
    Small,
    Medium,
    Large,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerPlacement {
    NearWall,
    FreeStanding,
}

impl EqProfile {
    /// Compute the effective gain for each band, combining the room correction
    /// preset with the user's perceptual adjustments.
    pub fn effective_gains(&self) -> (f64, f64, f64) {
        let (base_bass, base_mid, base_treble) = self.room_correction_preset();
        (
            base_bass + self.bass_gain_db,
            base_mid + self.mid_gain_db,
            base_treble + self.treble_gain_db,
        )
    }

    /// Room correction preset based on macro environment settings.
    /// Returns (bass_db, mid_db, treble_db) offsets.
    fn room_correction_preset(&self) -> (f64, f64, f64) {
        if self.listening == ListeningMode::Headphones {
            // Headphones: slight bass boost for missing physical impact,
            // slight treble rolloff for reduced fatigue
            return (1.5, 0.0, -1.0);
        }

        match (self.room_size, self.speaker_placement) {
            // Small room + near wall: strong bass buildup, reduce bass
            (RoomSize::Small, SpeakerPlacement::NearWall) => (-4.0, 0.5, 0.0),
            // Small room + free standing: moderate bass buildup
            (RoomSize::Small, SpeakerPlacement::FreeStanding) => (-2.0, 0.0, 0.5),
            // Medium room + near wall: some bass buildup
            (RoomSize::Medium, SpeakerPlacement::NearWall) => (-2.5, 0.0, 0.0),
            // Medium room + free standing: neutral (reference)
            (RoomSize::Medium, SpeakerPlacement::FreeStanding) => (0.0, 0.0, 0.0),
            // Large room + near wall: slight bass buildup, treble loss
            (RoomSize::Large, SpeakerPlacement::NearWall) => (-1.5, 0.0, 1.0),
            // Large room + free standing: bass rolls off, compensate
            (RoomSize::Large, SpeakerPlacement::FreeStanding) => (1.5, 0.0, 1.5),
        }
    }
}

/// Biquad filter coefficients (Direct Form I).
#[derive(Debug, Clone, Copy)]
struct BiquadCoeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// Biquad filter state (per channel).
#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    fn process(&mut self, c: &BiquadCoeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Design a low-shelf biquad filter.
fn low_shelf(freq: f64, gain_db: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / 2.0 * (2.0_f64).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

    let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    BiquadCoeffs {
        b0: (a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0,
        b1: (2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0,
        b2: (a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0,
        a1: (-2.0 * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0,
        a2: ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0,
    }
}

/// Design a peaking EQ biquad filter.
fn peaking_eq(freq: f64, gain_db: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / (2.0 * q);

    let a0 = 1.0 + alpha / a;
    BiquadCoeffs {
        b0: (1.0 + alpha * a) / a0,
        b1: (-2.0 * cos_w0) / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

/// Design a high-shelf biquad filter.
fn high_shelf(freq: f64, gain_db: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / 2.0 * (2.0_f64).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

    let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    BiquadCoeffs {
        b0: (a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0,
        b1: (-2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0,
        b2: (a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0,
        a1: (2.0 * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0,
        a2: ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0,
    }
}

/// Design a low-pass biquad filter (RBJ cookbook).
fn low_pass(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 - cos_w0) / 2.0) / a0,
        b1: (1.0 - cos_w0) / a0,
        b2: ((1.0 - cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Design a high-pass biquad filter (RBJ cookbook).
fn high_pass(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 + cos_w0) / 2.0) / a0,
        b1: (-(1.0 + cos_w0)) / a0,
        b2: ((1.0 + cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Design a notch biquad filter (RBJ cookbook).
fn notch(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: 1.0 / a0,
        b1: (-2.0 * cos_w0) / a0,
        b2: 1.0 / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// 3-band parametric EQ processor.
///
/// Processes interleaved PCM samples in-place. Supports any bit depth
/// (samples are converted to/from f64 internally).
pub struct EqProcessor {
    /// Biquad cascade: 3 tilt filters (historic profiler) or one per
    /// expert-mode band.
    filters: Vec<BiquadCoeffs>,
    /// Per-channel state for each cascade stage: [channel][stage]
    states: Vec<Vec<BiquadState>>,
    channels: u16,
    enabled: bool,
}

impl EqProcessor {
    /// Create a new EQ processor from a profile and sample rate.
    pub fn new(profile: &EqProfile, sample_rate: u32, channels: u16) -> Self {
        let sr = sample_rate as f64;

        // Expert-mode bands take over the whole cascade when present; the
        // 3-tilt profiler cascade is the fallback (unchanged behaviour).
        let filters: Vec<BiquadCoeffs> = if profile.bands.is_empty() {
            let (bass_db, mid_db, treble_db) = profile.effective_gains();
            if bass_db.abs() > 0.01 || mid_db.abs() > 0.01 || treble_db.abs() > 0.01 {
                vec![
                    low_shelf(80.0, bass_db, sr),
                    peaking_eq(2000.0, mid_db, 1.0, sr),
                    high_shelf(10000.0, treble_db, sr),
                ]
            } else {
                Vec::new()
            }
        } else {
            profile
                .bands
                .iter()
                .filter(|b| !b.is_neutral())
                .map(|b| b.coeffs(sr))
                .collect()
        };

        let states = vec![vec![BiquadState::default(); filters.len()]; channels as usize];
        let enabled = profile.enabled && !filters.is_empty();

        Self {
            filters,
            states,
            channels,
            enabled,
        }
    }

    /// Process interleaved PCM bytes in-place.
    /// `bit_depth`: 16, 24, or 32.
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16) {
        if !self.enabled || pcm.is_empty() || self.channels == 0 {
            return;
        }

        let bytes_per_sample = (bit_depth / 8) as usize;
        let frame_size = bytes_per_sample * self.channels as usize;

        for frame in pcm.chunks_exact_mut(frame_size) {
            for ch in 0..self.channels as usize {
                let offset = ch * bytes_per_sample;
                let sample = read_sample_f64(&frame[offset..], bytes_per_sample, bit_depth);

                let state = &mut self.states[ch];
                let mut s = sample;
                for (stage, coeffs) in state.iter_mut().zip(self.filters.iter()) {
                    s = stage.process(coeffs, s);
                }

                // Soft clip to prevent digital overs
                let out = soft_clip(s);
                write_sample_f64(&mut frame[offset..], out, bytes_per_sample, bit_depth);
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

fn read_sample_f64(buf: &[u8], bytes: usize, bit_depth: u16) -> f64 {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    let raw = match bytes {
        2 => i16::from_le_bytes([buf[0], buf[1]]) as f64,
        3 => {
            let val = buf[0] as i32 | (buf[1] as i32) << 8 | ((buf[2] as i8) as i32) << 16;
            val as f64
        }
        4 => i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as f64,
        _ => 0.0,
    };
    raw / max_val
}

fn write_sample_f64(buf: &mut [u8], sample: f64, bytes: usize, bit_depth: u16) {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    let clamped = sample.clamp(-1.0, 1.0 - f64::EPSILON);
    let raw = (clamped * max_val) as i64;
    match bytes {
        2 => {
            let b = (raw as i16).to_le_bytes();
            buf[0] = b[0];
            buf[1] = b[1];
        }
        3 => {
            buf[0] = raw as u8;
            buf[1] = (raw >> 8) as u8;
            buf[2] = (raw >> 16) as u8;
        }
        4 => {
            let b = (raw as i32).to_le_bytes();
            buf[0] = b[0];
            buf[1] = b[1];
            buf[2] = b[2];
            buf[3] = b[3];
        }
        _ => {}
    }
}

/// Soft clipper to prevent digital overs from EQ boost.
/// Uses tanh-based saturation above 0.95 for smooth limiting.
fn soft_clip(x: f64) -> f64 {
    if x.abs() < 0.95 {
        x
    } else {
        x.signum() * (0.95 + 0.05 * ((x.abs() - 0.95) / 0.05).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_eq_is_transparent() {
        let profile = EqProfile::default();
        let eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
    }

    #[test]
    fn boosted_eq_modifies_signal() {
        let profile = EqProfile {
            enabled: true,
            bass_gain_db: 6.0,
            mid_gain_db: 0.0,
            treble_gain_db: 0.0,
            ..Default::default()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        assert!(eq.is_enabled());

        // Generate a 80Hz sine wave (2 channels, 16-bit, 1024 samples)
        let sr = 44100.0;
        let freq = 80.0;
        let mut pcm = Vec::with_capacity(1024 * 4);
        for i in 0..1024 {
            let sample = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            let s16 = (sample * 32767.0) as i16;
            pcm.extend_from_slice(&s16.to_le_bytes()); // L
            pcm.extend_from_slice(&s16.to_le_bytes()); // R
        }

        let original = pcm.clone();
        eq.process_pcm(&mut pcm, 16);

        // Signal should be modified (boosted bass)
        assert_ne!(pcm, original);
    }

    #[test]
    fn neutral_bands_are_transparent() {
        // Expert-mode bands all flat → no cascade, EQ reports disabled.
        let profile = EqProfile {
            enabled: true,
            bands: vec![
                EqBandSpec {
                    freq: 1000.0,
                    gain: 0.0,
                    q: 1.0,
                    band_type: "peak".into(),
                },
                EqBandSpec {
                    freq: 100.0,
                    gain: 0.0,
                    q: 1.0,
                    band_type: "low_shelf".into(),
                },
            ],
            ..Default::default()
        };
        let eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
    }

    #[test]
    fn band_high_shelf_attenuates_treble() {
        // -12 dB high shelf at 2 kHz must attenuate an 8 kHz sine strongly.
        let profile = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 2000.0,
                gain: -12.0,
                q: 0.71,
                band_type: "high_shelf".into(),
            }],
            ..Default::default()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 1);
        assert!(eq.is_enabled());

        let sr = 44100.0;
        let freq = 8000.0;
        let mut pcm = Vec::with_capacity(4096 * 2);
        for i in 0..4096 {
            let sample = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            let s16 = (sample * 32767.0) as i16;
            pcm.extend_from_slice(&s16.to_le_bytes());
        }
        let rms = |buf: &[u8]| {
            let mut acc = 0.0f64;
            let mut n = 0usize;
            for c in buf.chunks_exact(2) {
                let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
                acc += v * v;
                n += 1;
            }
            (acc / n as f64).sqrt()
        };
        let rms_before = rms(&pcm);
        eq.process_pcm(&mut pcm, 16);
        // Skip the first 512 samples (filter settle) for the RMS check.
        let rms_after = rms(&pcm[1024..]);
        let delta_db = 20.0 * (rms_after / rms_before).log10();
        assert!(
            delta_db < -9.0,
            "expected ~-12 dB at 8 kHz, got {delta_db:.2} dB"
        );
    }

    #[test]
    fn room_correction_presets() {
        let mut p = EqProfile::default();

        p.room_size = RoomSize::Small;
        p.speaker_placement = SpeakerPlacement::NearWall;
        let (bass, _, _) = p.room_correction_preset();
        assert!(bass < 0.0, "small room near wall should cut bass");

        p.room_size = RoomSize::Large;
        p.speaker_placement = SpeakerPlacement::FreeStanding;
        let (bass, _, treble) = p.room_correction_preset();
        assert!(bass > 0.0, "large room freestanding should boost bass");
        assert!(treble > 0.0, "large room should boost treble");
    }

    #[test]
    fn soft_clip_preserves_normal_signal() {
        assert!((soft_clip(0.5) - 0.5).abs() < 1e-10);
        assert!((soft_clip(-0.5) - (-0.5)).abs() < 1e-10);
    }

    #[test]
    fn soft_clip_limits_overs() {
        assert!(soft_clip(1.5) < 1.0);
        assert!(soft_clip(-1.5) > -1.0);
    }
}
