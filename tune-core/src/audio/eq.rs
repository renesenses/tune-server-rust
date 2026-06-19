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

/// 3-band parametric EQ processor.
///
/// Processes interleaved PCM samples in-place. Supports any bit depth
/// (samples are converted to/from f64 internally).
pub struct EqProcessor {
    low: BiquadCoeffs,
    mid: BiquadCoeffs,
    high: BiquadCoeffs,
    /// Per-channel state for each band: [channel][band]
    states: Vec<[BiquadState; 3]>,
    channels: u16,
    enabled: bool,
}

impl EqProcessor {
    /// Create a new EQ processor from a profile and sample rate.
    pub fn new(profile: &EqProfile, sample_rate: u32, channels: u16) -> Self {
        let (bass_db, mid_db, treble_db) = profile.effective_gains();
        let sr = sample_rate as f64;

        let low = low_shelf(80.0, bass_db, sr);
        let mid = peaking_eq(2000.0, mid_db, 1.0, sr);
        let high = high_shelf(10000.0, treble_db, sr);

        let states = vec![[BiquadState::default(); 3]; channels as usize];

        Self {
            low,
            mid,
            high,
            states,
            channels,
            enabled: profile.enabled
                && (bass_db.abs() > 0.01 || mid_db.abs() > 0.01 || treble_db.abs() > 0.01),
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
                let s1 = state[0].process(&self.low, sample);
                let s2 = state[1].process(&self.mid, s1);
                let s3 = state[2].process(&self.high, s2);

                // Soft clip to prevent digital overs
                let out = soft_clip(s3);
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
