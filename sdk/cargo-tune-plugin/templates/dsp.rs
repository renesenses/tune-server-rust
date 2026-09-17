//! Example gain processor. Replace the algorithm, keep the contract tests.
use tune_plugin_sdk::{Error, Settings, audio::*};

pub struct Plugin;
struct Gain {
    gain: f32,
    format: AudioFormat,
    max_frames: usize,
}

fn gain(settings: &Settings) -> Result<f32, Error> {
    let value = settings
        .get("gain")
        .and_then(|v| v.as_f64())
        .ok_or(Error::InvalidSettings)?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(Error::InvalidSettings);
    }
    Ok(value as f32)
}

impl DspFactory for Plugin {
    fn assess(
        &self,
        _context: &PlaybackContext,
        settings: &Settings,
    ) -> Result<Applicability, Error> {
        Ok(if gain(settings)? == 1.0 {
            Applicability::Bypass(BypassReason::Neutral)
        } else {
            Applicability::Process { requires_pcm: true }
        })
    }
    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        settings: &Settings,
    ) -> Result<Box<dyn Processor>, Error> {
        if format.encoding() != SampleEncoding::F32 {
            return Err(Error::UnsupportedFormat);
        }
        if max_frames == 0 {
            return Err(Error::BlockTooLarge);
        }
        Ok(Box::new(Gain {
            gain: gain(settings)?,
            format,
            max_frames,
        }))
    }
}

impl Processor for Gain {
    fn process(
        &mut self,
        block: &mut AudioBlock<'_>,
        _context: BlockContext,
    ) -> Result<ProcessReport, Error> {
        if block.format() != self.format {
            return Err(Error::InvalidFormat);
        }
        if block.frames() > self.max_frames {
            return Err(Error::BlockTooLarge);
        }
        let SamplesMut::F32(samples) = block.samples_mut() else {
            return Err(Error::UnsupportedFormat);
        };
        // Validate the whole block before modifying it, so failure is atomic.
        if samples.iter().any(|s| !s.is_finite()) {
            return Err(Error::NonFinite);
        }
        for sample in samples.iter_mut() {
            *sample *= self.gain;
        }
        Ok(ProcessReport {
            changed: self.gain != 1.0,
            ..Default::default()
        })
    }
    fn reset(&mut self, _reason: ResetReason) {}
    fn latency_frames(&self) -> u32 {
        0
    }
    fn drain(&mut self, _block: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}
