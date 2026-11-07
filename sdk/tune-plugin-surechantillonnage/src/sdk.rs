//! #7614 — the upsampler as an SDK rate-changing stage.
//!
//! Disabled by default: with default settings `assess` answers
//! `Bypass(Disabled)` and nothing in the stream changes. Under PURE or a
//! protected bitstream the host policy bypasses it before any instance exists.
//!
//! Scope of this first stage (study § 7 and § 9 bis):
//! - integer ratios only, following the source family (44.1 → 352.8/705.6,
//!   48 → 384/768; Q5): the highest accepted rate that is an integer multiple
//!   of the source, at most ×16 and at most `max_rate_hz` when set;
//! - filters `resolution`, `polyvalent`, `precision` and their minimum-phase
//!   variants; `precision-longue` stays a bench filter until fast convolution
//!   exists (study § 6.2: below real time in direct polyphase on a Xeon core);
//! - f32 or f64 interleaved PCM in, the same encoding out; computation in f64.
//!   Quantization and dither stay in the sink (study step I4).
//!
//! `latency_frames` is the kernel's peak position, in output frames. `drain`
//! flushes exactly that many frames, so that the emitted stream, once its
//! delay is removed, lasts exactly as long as the input.
use crate::conception::{self, Conception};
use crate::polyphase::Surechantillonneur;
use serde::{Deserialize, Serialize};
use tune_plugin_audio_support::limiteur::{CompteurDuLimiteur, Limiteur};
use tune_plugin_sdk::audio::*;
use tune_plugin_sdk::rate::*;
use tune_plugin_sdk::{Error, Settings};

/// Filters offered to listeners. `precision-longue` is measured by the bench
/// but not offered: it does not run in real time without fast convolution.
pub const OFFERED_FILTERS: [&str; 6] = [
    "polyvalent",
    "polyvalent-pm",
    "precision",
    "precision-pm",
    "resolution",
    "resolution-pm",
];
pub const DEFAULT_FILTER: &str = "polyvalent";
/// Highest integer ratio: 44.1 → 705.6 and 48 → 768 kHz.
pub const MAX_INTEGER_RATIO: u32 = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpsamplerSettings {
    pub enabled: bool,
    pub filter: String,
    /// Ceiling on the output rate, in Hz; `None` = highest the sink accepts.
    pub max_rate_hz: Option<u32>,
    /// Study § 7, « cadence cible » : ×2, ×4, ×8 or ×16 at most; `None` =
    /// the highest the sink accepts (« maximum du périphérique »).
    pub max_ratio: Option<u32>,
}

impl Default for UpsamplerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            filter: DEFAULT_FILTER.to_string(),
            max_rate_hz: None,
            max_ratio: None,
        }
    }
}

pub fn settings(s: &Settings) -> Result<UpsamplerSettings, Error> {
    let s = if s.is_null() {
        UpsamplerSettings::default()
    } else {
        UpsamplerSettings::deserialize(s).map_err(|_| Error::InvalidSettings)?
    };
    if !OFFERED_FILTERS.contains(&s.filter.as_str()) {
        return Err(Error::InvalidSettings);
    }
    if s.max_rate_hz.is_some_and(|r| !(1..=768_000).contains(&r)) {
        return Err(Error::InvalidSettings);
    }
    if s.max_ratio.is_some_and(|r| !OFFERED_RATIOS.contains(&r)) {
        return Err(Error::InvalidSettings);
    }
    Ok(s)
}

/// The « cadence cible » choices of the study (§ 7), as integer ratios.
pub const OFFERED_RATIOS: [u32; 4] = [2, 4, 8, 16];

/// [`choose_rate`] with every ceiling the settings carry.
pub fn choose_rate_for(input_rate: u32, accepted: &[u32], s: &UpsamplerSettings) -> Option<u32> {
    let ratio_cap = s.max_ratio.and_then(|r| input_rate.checked_mul(r));
    let cap = match (s.max_rate_hz, ratio_cap) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    choose_rate(input_rate, accepted, cap)
}

/// Worst-case gain of an `l`-phase interpolator built on `kernel`: the
/// largest L1 norm among its phases, `max_p Σ_q |h[p + l·q]|`. No input
/// bounded by 1 can produce an output above this value, and some input
/// (the signs of the worst phase, reversed in time) reaches it.
pub fn worst_case_gain(kernel: &[f64], l: usize) -> f64 {
    if l == 0 {
        return 1.0;
    }
    (0..l)
        .map(|p| {
            kernel
                .iter()
                .skip(p)
                .step_by(l)
                .map(|c| c.abs())
                .sum::<f64>()
        })
        .fold(0.0, f64::max)
}

/// The fixed level reserve, in dB (Bertrand, 07/11, second decision): about
/// 3 dB, the inter-sample over of the classic worst musical case (fs/4 at
/// 45°, +3.01 dB). The L1 bound of the polyphase (5.7 to 16.5 dB depending on
/// the filter) is NOT applied any more: the safety limiter of the
/// equalizer's Realistic mode (`tune_plugin_audio_support::limiteur`) takes
/// the rarer, larger transients instead.
pub const RESERVE_DB: f64 = -3.0;

/// The gain the stage applies before its limiter, for `filter` at integer
/// ratio `ratio`: [`RESERVE_DB`], or less attenuation when the L1 bound of
/// the polyphase itself is smaller. Never above 1: the upsampler never raises
/// the level.
pub fn headroom_gain(filter: &str, ratio: u32) -> Option<f64> {
    l1_kernel(filter, ratio).map(|(kernel, l)| reserve_from_kernel(&kernel, l))
}

fn l1_kernel(filter: &str, ratio: u32) -> Option<(Vec<f64>, usize)> {
    if !OFFERED_FILTERS.contains(&filter) || !(2..=MAX_INTEGER_RATIO).contains(&ratio) {
        return None;
    }
    Some((
        conception::par_id(filter)?.noyau(ratio as usize),
        ratio as usize,
    ))
}

fn reserve_from_kernel(kernel: &[f64], l: usize) -> f64 {
    let l1 = 1.0 / worst_case_gain(kernel, l).max(1.0);
    l1.max(10f64.powf(RESERVE_DB / 20.0))
}

/// [`headroom_gain`] in dB (≤ 0).
pub fn headroom_db(filter: &str, ratio: u32) -> Option<f64> {
    headroom_gain(filter, ratio).map(|g| 20.0 * g.log10())
}

/// The reserve the L1 bound alone would need (the first decision of 07/11),
/// in dB, for comparison only.
pub fn l1_bound_db(filter: &str, ratio: u32) -> Option<f64> {
    l1_kernel(filter, ratio).map(|(kernel, l)| -20.0 * worst_case_gain(&kernel, l).max(1.0).log10())
}

/// The reference pair of the machine test (`machine::RAPPORTS`) whose
/// admission covers an output rate: the cost per output frame of an integer
/// polyphase is the kernel span whatever the ratio, so the pair with the
/// same or the next higher output rate of the same family is the bound.
pub fn machine_reference_pair(output_rate: u32) -> Option<(u32, u32)> {
    if output_rate.is_multiple_of(44_100) {
        Some(if output_rate <= 352_800 {
            (44_100, 352_800)
        } else {
            (44_100, 705_600)
        })
    } else if output_rate.is_multiple_of(48_000) {
        Some(if output_rate <= 384_000 {
            (48_000, 384_000)
        } else {
            (48_000, 768_000)
        })
    } else {
        None
    }
}

/// The machine test of the offered filters, on the four integer pairs.
pub fn machine_test(duration_s: f64, margin: f64) -> crate::machine::RapportMachine {
    let conceptions: Vec<_> = OFFERED_FILTERS
        .iter()
        .filter_map(|id| conception::par_id(id))
        .collect();
    let pairs: Vec<(u32, u32)> = crate::machine::RAPPORTS
        .iter()
        .copied()
        .filter(|(de, vers)| vers % de == 0)
        .collect();
    crate::machine::tester_la_machine_sur(&conceptions, &pairs, 2, duration_s, margin)
}

/// Admission margin of the study (§ 5): × 3 real time.
pub const MACHINE_MARGIN: f64 = 3.0;

/// The output rate chosen for `input_rate` among `accepted`, or `None`.
pub fn choose_rate(input_rate: u32, accepted: &[u32], max_rate_hz: Option<u32>) -> Option<u32> {
    accepted
        .iter()
        .copied()
        .filter(|&r| r > input_rate && input_rate > 0 && r % input_rate == 0)
        .filter(|&r| r / input_rate <= MAX_INTEGER_RATIO)
        .filter(|&r| max_rate_hz.is_none_or(|m| r <= m))
        .max()
}

pub struct Upsampler;

impl RateChangerFactory for Upsampler {
    fn assess(&self, ctx: &PlaybackContext, s: &Settings) -> Result<Applicability, Error> {
        if let Some(reason) = policy_bypass(ctx, true) {
            return Ok(Applicability::Bypass(reason));
        }
        Ok(if settings(s)?.enabled {
            Applicability::Process { requires_pcm: true }
        } else {
            Applicability::Bypass(BypassReason::Disabled)
        })
    }

    fn negotiate(
        &self,
        input: AudioFormat,
        accepted: &[u32],
        s: &Settings,
    ) -> Result<Option<u32>, Error> {
        let s = settings(s)?;
        if !s.enabled || !matches!(input.encoding(), SampleEncoding::F32 | SampleEncoding::F64) {
            return Ok(None);
        }
        Ok(choose_rate_for(input.sample_rate(), accepted, &s))
    }

    fn prepare(&self, request: RateRequest, s: &Settings) -> Result<Box<dyn RateChanger>, Error> {
        let s = settings(s)?;
        Ok(Box::new(Instance::new(request, &s.filter)?))
    }
}

/// One stream. All buffers are allocated here; `process` and `drain` only
/// push within reserved capacity.
pub struct Instance {
    request: RateRequest,
    conception: Conception,
    engine: Surechantillonneur,
    ratio: usize,
    channels: usize,
    latency: u32,
    /// [`headroom_gain`], fixed for the life of the instance.
    gain: f64,
    /// The equalizer's safety limiter (Realistic mode), at the output rate:
    /// exactly 1 below −0.2 dBFS, a linked gain above, never past
    /// −0.01 dBFS. Zero latency.
    limiter: Limiteur,
    input_f64: Vec<f64>,
    output_f64: Vec<f64>,
    /// Output frames still owed by `drain`.
    tail_left: u64,
    /// Frames computed by a drain step but not yet handed out.
    pending: usize,
    pending_offset: usize,
}

impl Instance {
    pub fn new(request: RateRequest, filter: &str) -> Result<Self, Error> {
        if !OFFERED_FILTERS.contains(&filter) {
            return Err(Error::InvalidSettings);
        }
        let encoding = request.input.encoding();
        if !matches!(encoding, SampleEncoding::F32 | SampleEncoding::F64) {
            return Err(Error::UnsupportedFormat);
        }
        let (de, vers) = (request.input.sample_rate(), request.output_rate);
        if choose_rate(de, &[vers], None) != Some(vers) {
            return Err(Error::UnsupportedFormat);
        }
        let conception = conception::par_id(filter).ok_or(Error::InvalidSettings)?;
        let channels = usize::from(request.input.channels());
        let engine = Surechantillonneur::nouveau(&conception, de, vers, channels)
            .map_err(|_| Error::UnsupportedFormat)?;
        let ratio = (vers / de) as usize;
        let peak = engine
            .noyau_etage1()
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map_or(0, |(i, _)| i);
        let latency = u32::try_from(peak).map_err(|_| Error::UnsupportedFormat)?;
        let gain = reserve_from_kernel(engine.noyau_etage1(), ratio);
        let max_in = request.max_input_frames;
        Ok(Self {
            conception,
            engine,
            ratio,
            channels,
            latency,
            gain,
            limiter: Limiteur::new(vers),
            input_f64: vec![0.0; max_in * channels],
            output_f64: Vec::with_capacity(max_in * ratio * channels),
            tail_left: u64::from(latency),
            pending: 0,
            pending_offset: 0,
            request,
        })
    }

    pub fn filter_id(&self) -> &'static str {
        self.conception.id
    }

    /// The constant gain applied to every output sample (≤ 1).
    pub fn gain(&self) -> f64 {
        self.gain
    }

    /// What the limiter did since the stream opened (or the last reset).
    pub fn limiter_counter(&self) -> CompteurDuLimiteur {
        self.limiter.compteur()
    }

    /// Reserve, then the linked limiter, frame by frame. With an attack in
    /// one sample, `|out| ≤ courbe(e) < PLAFOND` for every frame.
    fn write_out(&mut self, from: usize, frames: usize, output: &mut AudioBlock<'_>) {
        let ch = self.channels;
        let src = &self.output_f64[from * ch..(from + frames) * ch];
        let g = self.gain;
        let limiter = &mut self.limiter;
        let mut frame_gain = |frame: &[f64]| {
            let crete = frame.iter().fold(0.0f64, |m, s| m.max((s * g).abs()));
            g * limiter.gain(crete)
        };
        match output.samples_mut() {
            SamplesMut::F32(dst) => {
                for (d, s) in dst[..frames * ch]
                    .chunks_exact_mut(ch)
                    .zip(src.chunks_exact(ch))
                {
                    let k = frame_gain(s);
                    for (d, s) in d.iter_mut().zip(s) {
                        *d = (*s * k) as f32;
                    }
                }
            }
            SamplesMut::F64(dst) => {
                for (d, s) in dst[..frames * ch]
                    .chunks_exact_mut(ch)
                    .zip(src.chunks_exact(ch))
                {
                    let k = frame_gain(s);
                    for (d, s) in d.iter_mut().zip(s) {
                        *d = *s * k;
                    }
                }
            }
            _ => unreachable!("encoding checked in Instance::new"),
        }
    }
}

impl RateChanger for Instance {
    fn request(&self) -> RateRequest {
        self.request
    }

    fn process(
        &mut self,
        input: &AudioBlock<'_>,
        output: &mut AudioBlock<'_>,
        _: BlockContext,
    ) -> Result<RateReport, Error> {
        if input.format() != self.request.input || output.format() != self.output_format() {
            return Err(Error::InvalidFormat);
        }
        let frames = input.frames();
        if frames > self.request.max_input_frames || output.frames() < frames * self.ratio {
            return Err(Error::BlockTooLarge);
        }
        let ch = self.channels;
        let mut non_finite = 0u64;
        let staged = &mut self.input_f64[..frames * ch];
        match input.samples() {
            Samples::F32(src) => {
                for (d, s) in staged.iter_mut().zip(src) {
                    *d = if s.is_finite() {
                        f64::from(*s)
                    } else {
                        non_finite += 1;
                        0.0
                    };
                }
            }
            Samples::F64(src) => {
                for (d, s) in staged.iter_mut().zip(src) {
                    *d = if s.is_finite() {
                        *s
                    } else {
                        non_finite += 1;
                        0.0
                    };
                }
            }
            _ => return Err(Error::UnsupportedFormat),
        }
        self.output_f64.clear();
        self.engine
            .traiter(&self.input_f64[..frames * ch], &mut self.output_f64);
        let written = self.output_f64.len() / ch;
        debug_assert_eq!(written, frames * self.ratio);
        self.write_out(0, written, output);
        Ok(RateReport {
            frames_consumed: frames,
            frames_written: written,
            clipped_samples: 0,
            non_finite_samples: non_finite,
        })
    }

    fn reset(&mut self, _: ResetReason) {
        let (de, vers) = (self.request.input.sample_rate(), self.request.output_rate);
        // Rebuilding clears history and phase; it allocates, which `reset`
        // allows (control/producer thread, never the device callback).
        if let Ok(engine) = Surechantillonneur::nouveau(&self.conception, de, vers, self.channels) {
            self.engine = engine;
        }
        self.limiter.oublier();
        self.tail_left = u64::from(self.latency);
        self.pending = 0;
        self.pending_offset = 0;
    }

    fn latency_frames(&self) -> u32 {
        self.latency
    }

    fn drain(&mut self, output: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        if output.format() != self.output_format() {
            return Err(Error::InvalidFormat);
        }
        let capacity = output.frames();
        if capacity == 0 {
            return Err(Error::BlockTooLarge);
        }
        if self.pending == 0 && self.tail_left > 0 {
            // Feed silence: one input frame yields `ratio` output frames.
            let want = self.tail_left.min(capacity as u64) as usize;
            let feed = want.div_ceil(self.ratio).min(self.request.max_input_frames);
            let ch = self.channels;
            self.input_f64[..feed * ch].fill(0.0);
            self.output_f64.clear();
            self.engine
                .traiter(&self.input_f64[..feed * ch], &mut self.output_f64);
            self.pending = (self.output_f64.len() / ch).min(self.tail_left as usize);
            self.pending_offset = 0;
        }
        let n = self.pending.min(capacity);
        self.write_out(self.pending_offset, n, output);
        self.pending -= n;
        self.pending_offset += n;
        self.tail_left -= n as u64;
        Ok(DrainReport {
            frames_written: n,
            complete: self.tail_left == 0,
        })
    }

    fn diagnostics(&self) -> Settings {
        serde_json::json!({
            "filter": self.conception.id,
            "from_hz": self.request.input.sample_rate(),
            "to_hz": self.request.output_rate,
            "latency_output_frames": self.latency,
            "latency_ms": f64::from(self.latency) * 1000.0 / f64::from(self.request.output_rate),
            "headroom_db": 20.0 * self.gain.log10(),
            "limited_frames": self.limiter.compteur().trames_limitees,
            "limiter_max_reduction_db": self.limiter.compteur().reduction_max_db,
        })
    }
}
