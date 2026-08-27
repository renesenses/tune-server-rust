//! Parametric equalizer for the Tune Master Profiler.
//!
//! 3-band EQ using biquad filters (Robert Bristow-Johnson Audio EQ Cookbook):
//! - Low shelf (60-80 Hz) — bass weight
//! - Mid peak (1-3 kHz) — voice presence/clarity
//! - High shelf (10-12 kHz) — treble air/brightness
//!
//! Coefficients and filter state are computed in f64. That is a claim about
//! ARITHMETIC precision and nothing else: the biquad accumulators stay far
//! below the noise floor of 16- and 24-bit material, so the filter adds no
//! audible rounding of its own. It is NOT a claim of bit-perfection — an
//! active equalizer modifies every sample, by design, whatever the width of
//! its accumulators. The two properties are independent and Tune must not
//! trade the wording of one for the other (#2213): the signal-path panel
//! already flips `bit_perfect` to false as soon as the EQ alters the signal
//! (`tune-server/src/routes/zones.rs`, `zone_eq_alters_signal`). Disabled — or
//! in PURE mode, where the processor is never built — this stage is a strict
//! identity and the samples pass through untouched.
//!
//! The EQ profile is stored per-zone and applied in the PCM pipeline before
//! output.

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
    /// Le canal auquel cette bande s'applique. `None` = TOUS les canaux.
    ///
    /// C'est le defaut, et c'est ce qui rend les preregla­ges existants
    /// inchanges : un profil enregistre avant cette version n'a pas ce champ,
    /// `serde` le laisse a `None`, et la bande s'applique partout — exactement
    /// comme avant.
    ///
    /// `Some(0)` = gauche, `Some(1)` = droite. Une piece dissymetrique — un mur
    /// d'un cote, une ouverture de l'autre — ne se corrige pas avec la meme
    /// courbe des deux cotes : c'est la demande d'Alexander Jam, abonne
    /// Premium, qui venait chercher l'equivalent de ce que fait Roon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
}

impl Default for EqBandSpec {
    /// Une bande neutre, sur tous les canaux. Permet aux appelants d'ecrire
    /// `..Default::default()` et de ne plus casser quand une option s'ajoute —
    /// c'est exactement ce qui vient d'arriver avec `channel`.
    fn default() -> Self {
        Self {
            freq: 1000.0,
            gain: 0.0,
            q: default_band_q(),
            band_type: default_band_type(),
            channel: None,
        }
    }
}

impl EqBandSpec {
    /// Cette bande agit-elle sur ce canal ?
    fn vise_le_canal(&self, ch: u16) -> bool {
        match self.channel {
            None => true,
            Some(c) => c == ch,
        }
    }
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
    /// Compute the effective gain for each band, combining the environment
    /// tone preset with the user's perceptual adjustments.
    pub fn effective_gains(&self) -> (f64, f64, f64) {
        let (base_bass, base_mid, base_treble) = self.environment_tone_preset();
        (
            base_bass + self.bass_gain_db,
            base_mid + self.mid_gain_db,
            base_treble + self.treble_gain_db,
        )
    }

    /// Tone preset for the DECLARED listening environment.
    /// Returns (bass_db, mid_db, treble_db) offsets.
    ///
    /// Six hard-coded tilts, chosen from two enums the listener picks in a
    /// three-question wizard — room size and speaker placement — plus a
    /// headphone case. **Nothing is measured here.** No microphone, no sweep,
    /// no impulse response, no frequency response of the room or of the
    /// speakers ever reaches this function; two rooms answering the same three
    /// questions get the same three numbers.
    ///
    /// The name this function carried until #2213 promised something the code
    /// does not do. In audiophile usage, "room correction" means a filter
    /// DERIVED FROM AN ACOUSTIC MEASUREMENT — which Tune does offer,
    /// elsewhere: `crate::room_correction` stores a `RoomProfile` with its
    /// `measurement_data`, and `crate::audio::convolver` convolves a WAV
    /// impulse response exported from REW, Acourate or Audiolense. Those
    /// deserve the term; this table does not.
    ///
    /// What it is worth is stated plainly: a sane starting tilt for a stated
    /// environment, to be finished by ear with the three sliders.
    fn environment_tone_preset(&self) -> (f64, f64, f64) {
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
#[derive(Debug, Clone, Copy, Default, PartialEq)]
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
    /// Cascade biquad PAR CANAL : `[canal][etage]`.
    ///
    /// Elle etait partagee — les memes coefficients pour tous les canaux. Une
    /// bande peut desormais viser un canal (#Alexander Jam), et gauche et
    /// droite n'ont donc plus forcement la meme courbe.
    filters: Vec<Vec<BiquadCoeffs>>,
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
        // La cascade du profileur historique (3 filtres de tilt) ne connait pas
        // les canaux : elle s'applique partout, a l'identique. Seules les
        // bandes du mode expert peuvent viser un canal.
        let commune: Vec<BiquadCoeffs> = if profile.bands.is_empty() {
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
            Vec::new()
        };

        let filters: Vec<Vec<BiquadCoeffs>> = (0..channels.max(1))
            .map(|ch| {
                if profile.bands.is_empty() {
                    commune.clone()
                } else {
                    profile
                        .bands
                        .iter()
                        .filter(|b| !b.is_neutral() && b.vise_le_canal(ch))
                        .map(|b| b.coeffs(sr))
                        .collect()
                }
            })
            .collect();

        let states = filters
            .iter()
            .map(|f| vec![BiquadState::default(); f.len()])
            .collect();
        // Un profil dont TOUTES les bandes sont neutres, ou qui ne vise aucun
        // canal existant, ne doit pas rester « actif » a ne rien faire.
        let enabled = profile.enabled && filters.iter().any(|f| !f.is_empty());

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
                let cascade = &self.filters[ch];
                let mut s = sample;
                for (stage, coeffs) in state.iter_mut().zip(cascade.iter()) {
                    s = stage.process(coeffs, s);
                }

                // Soft clip to prevent digital overs
                let out = soft_clip(s);
                write_sample_f64(&mut frame[offset..], out, bytes_per_sample, bit_depth);
            }
        }
    }

    /// Process an **interleaved f32** buffer (`[L0, R0, L1, R1, …]`, normalised
    /// to -1..1) in place.
    ///
    /// Same cascade, same per-channel state and same soft-clip as
    /// [`Self::process_pcm`] — only the sample representation differs. The
    /// local output (`outputs/local.rs`) already holds its audio as f32 for
    /// cpal and runs its convolver / crossfeed on that buffer; going through
    /// the byte-oriented `process_pcm` there would mean packing to PCM and back
    /// on every chunk, in the audio hot path.
    ///
    /// A buffer that is not a whole number of frames is left untouched rather
    /// than processed half-way: a partial frame would advance the per-channel
    /// filter states out of step and every later chunk would be filtered with
    /// the wrong channel's history.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if !self.enabled || samples.is_empty() || self.channels == 0 {
            return;
        }
        let ch_count = self.channels as usize;
        if samples.len() % ch_count != 0 {
            return;
        }

        for frame in samples.chunks_exact_mut(ch_count) {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let state = &mut self.states[ch];
                let cascade = &self.filters[ch];
                let mut s = *sample as f64;
                for (stage, coeffs) in state.iter_mut().zip(cascade.iter()) {
                    s = stage.process(coeffs, s);
                }
                *sample = soft_clip(s) as f32;
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Reprendre l'historique des filtres du processeur que celui-ci remplace,
    /// quand la cascade a la même forme.
    ///
    /// Sert au remplacement **en cours de lecture** (#1725) : un curseur bougé
    /// pendant qu'un morceau joue. Un `EqProcessor` neuf part avec des
    /// `BiquadState` à zéro ; injecter un signal continu dans un filtre dont
    /// l'historique vient d'être remis à zéro est une discontinuité, et une
    /// discontinuité dans le chemin audio, c'est un clic. Un curseur qu'on
    /// fait glisser en produirait un à chaque cran.
    ///
    /// L'état n'est transposable que si la cascade coïncide en nombre de
    /// canaux ET d'étages, parce que `states[canal][étage]` est positionnel :
    /// l'étage *n* de l'ancienne cascade n'est le même filtre que l'étage *n*
    /// de la nouvelle que si rien n'a été inséré ni retiré. Changer le gain ou
    /// la fréquence d'une bande conserve la forme — les coefficients changent,
    /// pas la cascade — et c'est justement le cas courant, celui du glissement.
    ///
    /// Quand la forme change — une bande qui repasse sous le seuil
    /// d'audibilité sort de la cascade via `is_neutral()`, donc le nombre
    /// d'étages bouge — l'historique n'est pas transposable et reste à zéro. Ce
    /// transitoire-là est inévitable, et c'est le même qu'on entend déjà au
    /// début de chaque piste.
    pub fn inherit_state_from(&mut self, previous: &EqProcessor) {
        // `filters` compte desormais les CANAUX, plus les etages : comparer sa
        // longueur ne dit plus rien de la forme de la cascade. Il faut comparer
        // canal par canal, sinon on recopierait l'etat d'une cascade a une
        // autre qui n'a pas les memes etages — c'est-a-dire exactement ce que
        // ce garde-fou existe pour empecher.
        //
        // Et depuis que les bandes peuvent viser un canal, deux canaux du meme
        // profil n'ont pas forcement la meme longueur de cascade : la
        // comparaison DOIT etre par canal.
        if previous.channels != self.channels
            || previous.filters.len() != self.filters.len()
            || previous
                .filters
                .iter()
                .zip(self.filters.iter())
                .any(|(a, b)| a.len() != b.len())
        {
            return;
        }
        self.states.clone_from(&previous.states);
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
                    ..Default::default()
                },
                EqBandSpec {
                    freq: 100.0,
                    gain: 0.0,
                    q: 1.0,
                    band_type: "low_shelf".into(),
                    ..Default::default()
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
                ..Default::default()
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
    fn environment_tone_presets() {
        let mut p = EqProfile::default();

        p.room_size = RoomSize::Small;
        p.speaker_placement = SpeakerPlacement::NearWall;
        let (bass, _, _) = p.environment_tone_preset();
        assert!(bass < 0.0, "small room near wall should cut bass");

        p.room_size = RoomSize::Large;
        p.speaker_placement = SpeakerPlacement::FreeStanding;
        let (bass, _, treble) = p.environment_tone_preset();
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

    /// Profil de test : -12 dB de plateau aigu à 2 kHz, stéréo.
    fn shelf_profile() -> EqProfile {
        EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 2000.0,
                gain: -12.0,
                q: 0.71,
                band_type: "high_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Sinus stéréo entrelacé, en f32 et en PCM 32 bits, échantillon pour
    /// échantillon.
    fn stereo_sine(freq: f64, frames: usize) -> (Vec<f32>, Vec<u8>) {
        let sr = 44100.0;
        let mut f32s = Vec::with_capacity(frames * 2);
        let mut pcm = Vec::with_capacity(frames * 8);
        for i in 0..frames {
            let v = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            // Aller-retour par l'entier 32 bits AVANT de remplir les deux
            // tampons : sans ça la comparaison mesurerait l'erreur de
            // quantification du PCM, pas l'égalité des deux chemins.
            let raw = (v * 2147483648.0) as i32;
            let q = raw as f64 / 2147483648.0;
            for _ in 0..2 {
                f32s.push(q as f32);
                pcm.extend_from_slice(&raw.to_le_bytes());
            }
        }
        (f32s, pcm)
    }

    /// Le chemin f32 (sortie locale) et le chemin PCM (transcodage) doivent
    /// donner le MÊME signal : c'est ce qui permet à une zone d'entendre la
    /// même correction sur son DAC et vers un renderer réseau.
    #[test]
    fn interleaved_matches_pcm_path() {
        let profile = shelf_profile();
        let (mut f32s, mut pcm) = stereo_sine(8000.0, 2048);

        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut f32s);
        EqProcessor::new(&profile, 44100, 2).process_pcm(&mut pcm, 32);

        assert_eq!(f32s.len() * 4, pcm.len());
        for (i, (got, chunk)) in f32s.iter().zip(pcm.chunks_exact(4)).enumerate() {
            let expected =
                i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32 / 2147483648.0;
            assert!(
                (got - expected).abs() < 1e-6,
                "échantillon {i} : f32={got} pcm={expected}"
            );
        }
    }

    /// Les deux canaux ont leur propre état de biquad : un signal présent à
    /// gauche seulement ne doit pas colorer la droite. Une erreur d'indice
    /// dans `process_interleaved` se verrait ici et nulle part ailleurs.
    #[test]
    fn interleaved_keeps_channels_independent() {
        let profile = shelf_profile();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        let mut samples = vec![0.0f32; 2048];
        for f in 0..1024 {
            samples[2 * f] = ((2.0 * PI * 8000.0 * f as f64 / 44100.0).sin() * 0.5) as f32;
        }
        eq.process_interleaved(&mut samples);
        for f in 0..1024 {
            assert_eq!(samples[2 * f + 1], 0.0, "canal droit sali à la trame {f}");
        }
        assert!(samples.iter().step_by(2).any(|&s| s != 0.0));
    }

    /// Un profil désactivé (ou en mode PURE, que `load_eq_processor` traduit
    /// par `None`) laisse le signal strictement intact — la promesse
    /// bit-perfect.
    #[test]
    fn interleaved_is_identity_when_disabled() {
        let profile = EqProfile {
            enabled: false,
            ..shelf_profile()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
        let (mut samples, _) = stereo_sine(1000.0, 256);
        let before = samples.clone();
        eq.process_interleaved(&mut samples);
        assert_eq!(samples, before);
    }

    /// Un tampon qui ne contient pas un nombre entier de trames est laissé
    /// intact : le traiter à moitié décalerait l'état des filtres d'un canal
    /// et toutes les trames suivantes seraient filtrées avec le mauvais
    /// historique.
    #[test]
    fn interleaved_ignores_partial_frame() {
        let profile = shelf_profile();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        let mut samples = vec![0.5f32; 7]; // 3 trames + 1 échantillon
        let before = samples.clone();
        eq.process_interleaved(&mut samples);
        assert_eq!(samples, before);
    }

    /// L'état des biquads persiste d'un appel à l'autre : la sortie d'un
    /// tampon découpé en morceaux est identique à celle du tampon entier.
    /// C'est la condition pour que la sortie locale, qui reçoit l'audio par
    /// paquets de taille arbitraire, ne craque pas aux jointures.
    #[test]
    fn interleaved_state_persists_across_chunks() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);

        let mut one_shot = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut one_shot);

        let mut chunked = whole.clone();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        for chunk in chunked.chunks_mut(200 * 2) {
            eq.process_interleaved(chunk);
        }

        assert_eq!(one_shot, chunked);
    }

    /// Même profil, même forme de cascade : reprendre l'état revient exactement
    /// au même que n'avoir jamais changé de processeur. C'est LA garantie qui
    /// autorise à remplacer l'égaliseur en pleine lecture sans que ça s'entende.
    #[test]
    fn inherited_state_continues_the_stream_exactly() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);
        let split = 400 * 2; // frontière du remplacement, en échantillons

        // Référence : un seul processeur, du début à la fin.
        let mut reference = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut reference);

        // Remplacement à chaud : un processeur neuf reprend l'historique.
        let mut swapped = whole.clone();
        let mut first = EqProcessor::new(&profile, 44100, 2);
        first.process_interleaved(&mut swapped[..split]);
        let mut second = EqProcessor::new(&profile, 44100, 2);
        second.inherit_state_from(&first);
        second.process_interleaved(&mut swapped[split..]);

        assert_eq!(reference, swapped);
    }

    /// Sans reprise d'état, le même remplacement dévie du flux continu — c'est
    /// le clic. Ce test existe pour que la reprise d'état ne puisse pas
    /// disparaître en silence lors d'un remaniement.
    #[test]
    fn dropping_state_breaks_the_stream_where_inheriting_does_not() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);
        let split = 400 * 2;

        let mut reference = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut reference);

        let mut naive = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut naive[..split]);
        // Processeur neuf, historique à zéro : le comportement d'un `set_eq`
        // nu en cours de lecture.
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut naive[split..]);

        let ecart = (naive[split] - reference[split]).abs();
        assert!(
            ecart > 1e-6,
            "un historique perdu devrait dévier du flux continu (écart {ecart})"
        );
    }

    /// La cascade change de forme (une bande s'ajoute) : l'état n'est plus
    /// transposable et ne doit PAS être recopié sur des étages qui ne se
    /// correspondent plus.
    #[test]
    fn state_is_not_inherited_across_a_different_cascade() {
        let one_band = shelf_profile();
        let mut two_bands = shelf_profile();
        two_bands.bands.push(EqBandSpec {
            freq: 120.0,
            gain: 6.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        });

        let (mut buf, _) = stereo_sine(8000.0, 256);
        let mut warmed = EqProcessor::new(&one_band, 44100, 2);
        warmed.process_interleaved(&mut buf);

        let mut fresh = EqProcessor::new(&two_bands, 44100, 2);
        fresh.inherit_state_from(&warmed);

        assert_eq!(
            fresh.states,
            EqProcessor::new(&two_bands, 44100, 2).states,
            "état repris d'une cascade de taille différente"
        );
    }

    /// Le nombre de canaux est l'autre garde-fou : reprendre l'état d'une
    /// cascade stéréo sur une cascade mono écraserait la structure.
    #[test]
    fn state_is_not_inherited_across_a_channel_count_change() {
        let profile = shelf_profile();
        let mut stereo = EqProcessor::new(&profile, 44100, 2);
        let (mut buf, _) = stereo_sine(8000.0, 256);
        stereo.process_interleaved(&mut buf);

        let mut mono = EqProcessor::new(&profile, 44100, 1);
        mono.inherit_state_from(&stereo);

        assert_eq!(mono.states.len(), 1, "le nombre de canaux a été écrasé");
        assert_eq!(mono.states, EqProcessor::new(&profile, 44100, 1).states);
    }

    // --- Une bande peut ne viser QU'UN canal (Alexander Jam, Premium) ---

    fn bande(freq: f64, gain: f64, canal: Option<u16>) -> EqBandSpec {
        EqBandSpec {
            freq,
            gain,
            q: 0.71,
            band_type: "peak".into(),
            channel: canal,
        }
    }

    fn energie(samples: &[f32], pas: usize, depart: usize) -> f64 {
        samples
            .iter()
            .skip(depart)
            .step_by(pas)
            .map(|v| (*v as f64) * (*v as f64))
            .sum()
    }

    /// Le coeur de la demande : une piece dissymetrique se corrige d'un seul
    /// cote. Avant, la meme courbe partait a gauche ET a droite — l'egaliseur
    /// ne pouvait pas rattraper un desequilibre, ce pour quoi cet abonne avait
    /// paye.
    #[test]
    fn une_bande_ne_touche_que_son_canal() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, 12.0, Some(0))],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant_g = energie(&buf, 2, 0);
        let avant_d = energie(&buf, 2, 1);

        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);

        let apres_g = energie(&buf, 2, 0);
        let apres_d = energie(&buf, 2, 1);
        assert!(
            apres_g > avant_g * 1.5,
            "la gauche doit etre relevee : {avant_g} -> {apres_g}"
        );
        assert!(
            (apres_d - avant_d).abs() / avant_d < 0.01,
            "la droite doit rester intacte : {avant_d} -> {apres_d}"
        );
    }

    /// Deux courbes differentes, une par canal — le cas reel d'une piece dont
    /// un seul cote resonne.
    #[test]
    fn chaque_canal_peut_avoir_sa_propre_courbe() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, 12.0, Some(0)), bande(1000.0, -12.0, Some(1))],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant = energie(&buf, 2, 0);
        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);
        assert!(energie(&buf, 2, 0) > avant, "gauche relevee");
        assert!(energie(&buf, 2, 1) < avant, "droite attenuee");
    }

    /// Le defaut ne change RIEN : un prereglage enregistre avant cette version
    /// n'a pas de champ `channel`, et doit se comporter exactement comme
    /// avant — sur les deux canaux.
    #[test]
    fn une_bande_sans_canal_agit_partout_comme_avant() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, 12.0, None)],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant = energie(&buf, 2, 0);
        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);
        let g = energie(&buf, 2, 0);
        let d = energie(&buf, 2, 1);
        assert!(
            g > avant * 1.5 && d > avant * 1.5,
            "les deux canaux montent"
        );
        assert!((g - d).abs() / g < 1e-6, "et de la meme facon : {g} vs {d}");
    }

    /// Un JSON d'avant cette version se relit sans `channel` — le champ est
    /// facultatif, et son absence vaut « tous les canaux ».
    #[test]
    fn un_prereglage_ancien_se_relit_sans_canal() {
        let json = r#"{"freq":100.0,"gain":3.0,"q":0.7,"type":"low_shelf"}"#;
        let b: EqBandSpec = serde_json::from_str(json).unwrap();
        assert_eq!(b.channel, None);
        assert!(b.vise_le_canal(0) && b.vise_le_canal(1));
        // Et il ne repart PAS avec un champ que le client ne connait pas.
        assert!(!serde_json::to_string(&b).unwrap().contains("channel"));
    }

    #[test]
    fn une_bande_qui_ne_vise_aucun_canal_existant_n_active_rien() {
        // Canal 5 sur une sortie stereo : le profil ne doit pas rester
        // « actif » a ne rien faire.
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, 12.0, Some(5))],
            ..Default::default()
        };
        assert!(!EqProcessor::new(&profil, 44100, 2).enabled);
    }
}
