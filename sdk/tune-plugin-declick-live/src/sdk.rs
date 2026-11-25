//! SDK stage: `DspFactory` / `Processor` around [`DeclickLive`].
use crate::engine::{self, Config, DeclickLive};
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::{Error, Settings, audio::*};

/// Source descriptor. Premium: the host's existing `declick` entitlement
/// (decision P1, 17/11/2026); the licensing system itself is unchanged.
pub const MANIFEST: &str = include_str!("../manifest.embedded.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeclickLiveSettings {
    pub enabled: bool,
    /// Detection threshold in robust standard deviations (4 to 30).
    pub threshold: f64,
    /// Lookahead, announced to the host as latency (10 to 20 ms).
    pub lookahead_ms: f64,
    /// Longest click repaired (0.05 to 2 ms).
    pub max_click_ms: f64,
}

impl Default for DeclickLiveSettings {
    fn default() -> Self {
        let c = Config::default();
        Self {
            enabled: false,
            threshold: c.threshold,
            lookahead_ms: c.lookahead_ms,
            max_click_ms: c.max_click_ms,
        }
    }
}

impl DeclickLiveSettings {
    pub fn config(&self) -> Config {
        Config {
            threshold: self.threshold,
            lookahead_ms: self.lookahead_ms,
            max_click_ms: self.max_click_ms,
        }
    }
}

pub fn settings(value: &Settings) -> Result<DeclickLiveSettings, Error> {
    let s = if value.is_null() {
        DeclickLiveSettings::default()
    } else {
        DeclickLiveSettings::deserialize(value).map_err(|_| Error::InvalidSettings)?
    };
    let ok = (engine::THRESHOLD_MIN..=engine::THRESHOLD_MAX).contains(&s.threshold)
        && (engine::LOOKAHEAD_MIN_MS..=engine::LOOKAHEAD_MAX_MS).contains(&s.lookahead_ms)
        && (engine::MAX_CLICK_MIN_MS..=engine::MAX_CLICK_MAX_MS).contains(&s.max_click_ms);
    if ok {
        Ok(s)
    } else {
        Err(Error::InvalidSettings)
    }
}

pub struct DeclickLiveFactory;

impl DspFactory for DeclickLiveFactory {
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

    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        s: &Settings,
    ) -> Result<Box<dyn Processor>, Error> {
        let s = settings(s)?;
        let channels = usize::from(format.channels());
        let count = max_frames
            .checked_mul(channels)
            .filter(|n| *n > 0 && *n <= 4 * 1024 * 1024)
            .ok_or(Error::BlockTooLarge)?;
        let engine = DeclickLive::new(format.sample_rate(), channels, s.config())
            .ok_or(Error::UnsupportedFormat)?;
        Ok(Box::new(Instance {
            engine,
            format,
            max_frames,
            scratch: vec![0.0; count],
        }))
    }
}

pub struct Instance {
    engine: DeclickLive,
    format: AudioFormat,
    max_frames: usize,
    scratch: Vec<f64>,
}

impl Instance {
    pub fn engine(&self) -> &DeclickLive {
        &self.engine
    }
}

fn to_f64(samples: Samples<'_>, out: &mut [f64]) -> Result<(), Error> {
    match samples {
        Samples::S16(s) => {
            for (v, x) in out.iter_mut().zip(s) {
                *v = f64::from(*x) / 32_768.0;
            }
        }
        Samples::S24Le(s) => {
            for (v, b) in out.iter_mut().zip(s.as_chunks::<3>().0) {
                *v = f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) / 8_388_608.0;
            }
        }
        Samples::S32(s) => {
            for (v, x) in out.iter_mut().zip(s) {
                *v = f64::from(*x) / 2_147_483_648.0;
            }
        }
        Samples::F32(s) => {
            if s.iter().any(|x| !x.is_finite()) {
                return Err(Error::NonFinite);
            }
            for (v, x) in out.iter_mut().zip(s) {
                *v = f64::from(*x);
            }
        }
        Samples::F64(s) => {
            if s.iter().any(|x| !x.is_finite()) {
                return Err(Error::NonFinite);
            }
            out[..s.len()].copy_from_slice(s);
        }
    }
    Ok(())
}

/// Round to nearest and saturate. Values that came in unchanged go back out
/// bit for bit: every integer sample is exactly representable in `f64`.
fn from_f64(values: &[f64], samples: SamplesMut<'_>) {
    match samples {
        SamplesMut::S16(s) => {
            for (x, v) in s.iter_mut().zip(values) {
                *x = (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
            }
        }
        SamplesMut::S24Le(s) => {
            for (b, v) in s.as_chunks_mut::<3>().0.iter_mut().zip(values) {
                let q = (v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                b.copy_from_slice(&q.to_le_bytes()[..3]);
            }
        }
        SamplesMut::S32(s) => {
            for (x, v) in s.iter_mut().zip(values) {
                *x = (v * 2_147_483_648.0)
                    .round()
                    .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
            }
        }
        SamplesMut::F32(s) => {
            for (x, v) in s.iter_mut().zip(values) {
                *x = *v as f32;
            }
        }
        SamplesMut::F64(s) => s.copy_from_slice(&values[..s.len()]),
    }
}

impl Processor for Instance {
    fn process(
        &mut self,
        block: &mut AudioBlock<'_>,
        _: BlockContext,
    ) -> Result<ProcessReport, Error> {
        if block.format() != self.format {
            return Err(Error::InvalidFormat);
        }
        if block.frames() > self.max_frames {
            return Err(Error::BlockTooLarge);
        }
        let n = block.frames() * usize::from(self.format.channels());
        let scratch = &mut self.scratch[..n];
        to_f64(block.samples(), scratch)?;
        self.engine.process(scratch);
        from_f64(scratch, block.samples_mut());
        // The output is the input delayed by the lookahead: always changed.
        Ok(ProcessReport {
            changed: true,
            ..Default::default()
        })
    }

    fn update(&mut self, value: &Settings) -> Result<(), Error> {
        let s = settings(value)?;
        self.engine
            .set_config(s.config())
            .map_err(|_| Error::InvalidState)
    }

    fn inherit_from(&mut self, previous: &dyn Processor) -> Result<(), Error> {
        let previous = (previous as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .ok_or(Error::InvalidState)?;
        if previous.format != self.format {
            return Err(Error::InvalidFormat);
        }
        self.engine
            .inherit_state_from(&previous.engine)
            .map_err(|_| Error::InvalidState)
    }

    fn diagnostics(&self) -> Settings {
        let s = self.engine.stats();
        let c = self.engine.config();
        serde_json::json!({
            "latency_frames": self.engine.latency_frames(),
            "threshold": c.threshold,
            "lookahead_ms": c.lookahead_ms,
            "max_click_ms": c.max_click_ms,
            "clicks_repaired": s.clicks_repaired,
            "samples_modified": s.samples_modified,
            "rejected_long": s.rejected_long,
            "rejected_transient": s.rejected_transient,
            "rejected_unresolved": s.rejected_unresolved,
        })
    }

    fn reset(&mut self, _: ResetReason) {
        self.engine.reset();
    }

    fn latency_frames(&self) -> u32 {
        self.engine.latency_frames() as u32
    }

    fn drain(&mut self, block: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        if block.format() != self.format {
            return Err(Error::InvalidFormat);
        }
        let ch = usize::from(self.format.channels());
        let n = block.frames().min(self.max_frames) * ch;
        let (written, complete) = self.engine.drain(&mut self.scratch[..n]);
        let values = &self.scratch[..written * ch];
        match block.samples_mut() {
            SamplesMut::S16(s) => from_f64(values, SamplesMut::S16(&mut s[..written * ch])),
            SamplesMut::S24Le(s) => from_f64(values, SamplesMut::S24Le(&mut s[..written * ch * 3])),
            SamplesMut::S32(s) => from_f64(values, SamplesMut::S32(&mut s[..written * ch])),
            SamplesMut::F32(s) => from_f64(values, SamplesMut::F32(&mut s[..written * ch])),
            SamplesMut::F64(s) => from_f64(values, SamplesMut::F64(&mut s[..written * ch])),
        }
        Ok(DrainReport {
            frames_written: written,
            complete,
        })
    }
}
