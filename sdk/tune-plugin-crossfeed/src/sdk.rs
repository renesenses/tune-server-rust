use crate::CrossfeedProcessor;
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::{Error, Settings, audio::*};

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossfeedSettings {
    pub enabled: bool,
    pub amount: f32,
    pub delay_ms: f32,
}
impl Default for CrossfeedSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            amount: 0.3,
            delay_ms: 0.3,
        }
    }
}
fn settings(s: &Settings) -> Result<CrossfeedSettings, Error> {
    let s = CrossfeedSettings::deserialize(s).map_err(|_| Error::InvalidSettings)?;
    if !s.amount.is_finite()
        || !s.delay_ms.is_finite()
        || !(0.0..=0.5).contains(&s.amount)
        || !(0.0..=5.0).contains(&s.delay_ms)
    {
        return Err(Error::InvalidSettings);
    }
    Ok(s)
}
pub struct Crossfeed;
impl DspFactory for Crossfeed {
    fn assess(&self, ctx: &PlaybackContext, s: &Settings) -> Result<Applicability, Error> {
        if let Some(reason) = policy_bypass(ctx, true) {
            return Ok(Applicability::Bypass(reason));
        }
        let s = settings(s)?;
        Ok(if !s.enabled {
            Applicability::Bypass(BypassReason::Disabled)
        } else if s.amount == 0.0 {
            Applicability::Bypass(BypassReason::Neutral)
        } else {
            Applicability::Process { requires_pcm: true }
        })
    }
    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        s: &Settings,
    ) -> Result<Box<dyn Processor>, Error> {
        if format.layout() != ChannelLayout::Stereo || format.encoding() == SampleEncoding::F64 {
            return Err(Error::UnsupportedFormat);
        }
        let count = max_frames
            .checked_mul(2)
            .filter(|n| *n > 0 && *n <= 4 * 1024 * 1024)
            .ok_or(Error::BlockTooLarge)?;
        let s = settings(s)?;
        Ok(Box::new(Instance {
            engine: CrossfeedProcessor::new(
                format.sample_rate(),
                if s.enabled { s.amount } else { 0.0 },
                s.delay_ms,
            ),
            settings: s,
            format,
            max_frames,
            scratch: vec![0.0; count],
        }))
    }
}
struct Instance {
    engine: CrossfeedProcessor,
    settings: CrossfeedSettings,
    format: AudioFormat,
    max_frames: usize,
    scratch: Vec<f32>,
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
        if self.engine.amount() == 0.0 {
            return Ok(ProcessReport::default());
        }
        let scratch = &mut self.scratch[..block.frames() * 2];
        match block.samples_mut() {
            SamplesMut::F32(s) => {
                if s.iter().any(|x| !x.is_finite()) {
                    return Err(Error::NonFinite);
                }
                self.engine.process_interleaved(s);
            }
            SamplesMut::S16(s) => {
                for (v, x) in scratch.iter_mut().zip(s.iter()) {
                    *v = *x as f32 / 32768.0;
                }
                self.engine.process_interleaved(scratch);
                for (x, v) in s.iter_mut().zip(scratch.iter()) {
                    *x = (v * 32767.0).round() as i16;
                }
            }
            SamplesMut::S24Le(s) => {
                for (v, b) in scratch.iter_mut().zip(s.as_chunks::<3>().0.iter()) {
                    *v = i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / 2147483648.0;
                }
                self.engine.process_interleaved(scratch);
                for (b, v) in s.as_chunks_mut::<3>().0.iter_mut().zip(scratch.iter()) {
                    b.copy_from_slice(&((v * 8_388_607.0).round() as i32).to_le_bytes()[..3]);
                }
            }
            SamplesMut::S32(s) => {
                for (v, x) in scratch.iter_mut().zip(s.iter()) {
                    *v = *x as f32 / 2147483648.0;
                }
                self.engine.process_interleaved(scratch);
                for (x, v) in s.iter_mut().zip(scratch.iter()) {
                    *x = (v * 2_147_483_647.0).round() as i32;
                }
            }
            SamplesMut::F64(_) => return Err(Error::UnsupportedFormat),
        }
        Ok(ProcessReport {
            changed: true,
            ..Default::default()
        })
    }
    fn update(&mut self, value: &Settings) -> Result<(), Error> {
        let s = settings(value)?;
        let mut next = CrossfeedProcessor::new(
            self.format.sample_rate(),
            if s.enabled { s.amount } else { 0.0 },
            s.delay_ms,
        );
        next.inherit_state_from(&self.engine);
        self.engine = next;
        self.settings = s;
        Ok(())
    }
    fn inherit_from(&mut self, previous: &dyn Processor) -> Result<(), Error> {
        let previous = (previous as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .ok_or(Error::InvalidState)?;
        if previous.format != self.format {
            return Err(Error::InvalidFormat);
        }
        self.engine.inherit_state_from(&previous.engine);
        Ok(())
    }
    fn diagnostics(&self) -> Settings {
        serde_json::json!({"amount":self.engine.amount(),"delay_samples":self.engine.delay_samples()})
    }
    fn reset(&mut self, _: ResetReason) {
        self.engine.reset_history();
    }
    fn latency_frames(&self) -> u32 {
        0
    } // dry path is immediate; cross-path delay is an effect, not transport latency
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}
