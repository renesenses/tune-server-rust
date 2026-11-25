//! Example gain processor computed in `f64` (TESS T05). Replace the algorithm,
//! keep the contract tests.
//!
//! The manifest requests `audio-f64` (optional): a Tune host that offers it
//! hands this plugin `F64` blocks unchanged. A host that predates it still
//! loads the plugin and only ever sends `F32`, so `F32` stays implemented.
//! Integer PCM (S16, packed S24, S32) is accepted too, because Tune's host
//! adapter prepares one instance per encoding it may meet.
use tune_plugin_sdk::{Error, Settings, audio::*};

pub struct Plugin;
struct Gain {
    gain: f64,
    format: AudioFormat,
    max_frames: usize,
}

fn gain(settings: &Settings) -> Result<f64, Error> {
    let value = settings
        .get("gain")
        .and_then(|v| v.as_f64())
        .ok_or(Error::InvalidSettings)?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(Error::InvalidSettings);
    }
    Ok(value)
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
        let g = self.gain;
        // Validate the whole block before modifying it, so failure is atomic.
        match block.samples_mut() {
            SamplesMut::F64(s) => {
                if s.iter().any(|v| !v.is_finite()) {
                    return Err(Error::NonFinite);
                }
                s.iter_mut().for_each(|v| *v *= g);
            }
            SamplesMut::F32(s) => {
                if s.iter().any(|v| !v.is_finite()) {
                    return Err(Error::NonFinite);
                }
                s.iter_mut().for_each(|v| *v = (f64::from(*v) * g) as f32);
            }
            SamplesMut::S16(s) => s
                .iter_mut()
                .for_each(|v| *v = (f64::from(*v) * g).round() as i16),
            SamplesMut::S32(s) => s
                .iter_mut()
                .for_each(|v| *v = (f64::from(*v) * g).round() as i32),
            SamplesMut::S24Le(b) => {
                for e in b.chunks_exact_mut(3) {
                    let v = i32::from_le_bytes([0, e[0], e[1], e[2]]) >> 8;
                    let o = ((f64::from(v) * g).round() as i32).to_le_bytes();
                    e.copy_from_slice(&o[..3]);
                }
            }
        }
        Ok(ProcessReport {
            changed: g != 1.0,
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
