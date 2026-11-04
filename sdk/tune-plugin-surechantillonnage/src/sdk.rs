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
}

impl Default for UpsamplerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            filter: DEFAULT_FILTER.to_string(),
            max_rate_hz: None,
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
    Ok(s)
}

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
        Ok(choose_rate(input.sample_rate(), accepted, s.max_rate_hz))
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
        let max_in = request.max_input_frames;
        Ok(Self {
            conception,
            engine,
            ratio,
            channels,
            latency,
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

    fn write_out(&self, from: usize, frames: usize, output: &mut AudioBlock<'_>) {
        let ch = self.channels;
        let src = &self.output_f64[from * ch..(from + frames) * ch];
        match output.samples_mut() {
            SamplesMut::F32(dst) => {
                for (d, s) in dst[..frames * ch].iter_mut().zip(src) {
                    *d = *s as f32;
                }
            }
            SamplesMut::F64(dst) => dst[..frames * ch].copy_from_slice(src),
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
        })
    }
}
