use crate::{EqProcessor, EqProfile};
use tune_plugin_sdk::{Error, Settings, audio::*};

/// Factory for the historical equalizer. Settings use the persisted EqProfile
/// schema: macros, graphic/parametric bands, channel selection and headroom.
pub struct Equalizer;
fn profile(settings: &Settings) -> Result<EqProfile, Error> {
    let p: EqProfile = serde_json_from_value(settings)?;
    if ![p.bass_gain_db, p.mid_gain_db, p.treble_gain_db]
        .iter()
        .all(|x| x.is_finite())
        || p.bands.len() > 256
        || p.bands
            .iter()
            .any(|b| !b.freq.is_finite() || !b.gain.is_finite() || !b.q.is_finite())
    {
        return Err(Error::InvalidSettings);
    }
    Ok(p)
}
fn serde_json_from_value(s: &Settings) -> Result<EqProfile, Error> {
    use serde::Deserialize;
    EqProfile::deserialize(s).map_err(|_| Error::InvalidSettings)
}
impl DspFactory for Equalizer {
    fn assess(&self, ctx: &PlaybackContext, settings: &Settings) -> Result<Applicability, Error> {
        if let Some(reason) = policy_bypass(ctx, true) {
            return Ok(Applicability::Bypass(reason));
        }
        let p = profile(settings)?;
        Ok(if !p.enabled {
            Applicability::Bypass(BypassReason::Disabled)
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
        if format.encoding() == SampleEncoding::F64 {
            return Err(Error::UnsupportedFormat);
        }
        let count = max_frames
            .checked_mul(usize::from(format.channels()))
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024)
            .ok_or(Error::BlockTooLarge)?;
        let p = profile(settings)?;
        Ok(Box::new(Instance {
            engine: EqProcessor::new(&p, format.sample_rate(), format.channels()),
            profile: p,
            format,
            max_frames,
            scratch: vec![0; count],
        }))
    }
}
struct Instance {
    engine: EqProcessor,
    profile: EqProfile,
    format: AudioFormat,
    max_frames: usize,
    scratch: Vec<u8>,
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
        let changed = self.engine.is_enabled();
        let stats = match block.samples_mut() {
            SamplesMut::F32(samples) => self.engine.process_interleaved(samples),
            SamplesMut::S24Le(samples) => self.engine.process_pcm(samples, 24),
            SamplesMut::S16(samples) => {
                let bytes = &mut self.scratch[..samples.len() * 2];
                for (s, b) in samples.iter().zip(bytes.as_chunks_mut::<2>().0.iter_mut()) {
                    b.copy_from_slice(&s.to_le_bytes());
                }
                let stats = self.engine.process_pcm(bytes, 16);
                for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<2>().0.iter()) {
                    *s = i16::from_le_bytes([b[0], b[1]]);
                }
                stats
            }
            SamplesMut::S32(samples) => {
                let bytes = &mut self.scratch[..samples.len() * 4];
                for (s, b) in samples.iter().zip(bytes.as_chunks_mut::<4>().0.iter_mut()) {
                    b.copy_from_slice(&s.to_le_bytes());
                }
                let stats = self.engine.process_pcm(bytes, 32);
                for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<4>().0.iter()) {
                    *s = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                }
                stats
            }
            SamplesMut::F64(_) => return Err(Error::UnsupportedFormat),
        };
        let clipping = self.engine.ecretage();
        Ok(ProcessReport {
            clipping: Some(ClippingStats {
                samples_seen: clipping.echantillons_vus,
                clipped_samples: clipping.echantillons_ecretes,
                max_excess_lsb: clipping.exces_max_lsb,
                max_peak_bits: clipping.crete_max.to_bits(),
                first_clip: clipping.premier_ecretage_a,
            }),
            changed,
            clipped_samples: stats.overs,
            non_finite_samples: stats.non_finite_samples,
        })
    }
    fn update(&mut self, settings: &Settings) -> Result<(), Error> {
        let p = profile(settings)?;
        let mut next = EqProcessor::new(&p, self.format.sample_rate(), self.format.channels());
        next.inherit_state_from(&self.engine);
        self.engine = next;
        self.profile = p;
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
        let c = self.engine.ecretage();
        let stats = self.engine.process_stats();
        serde_json::json!({"response":self.engine.response(self.format.sample_rate()),"enabled":self.engine.is_enabled(),"preamp_db":(0..self.format.channels()).map(|ch|self.engine.preamp_db(ch)).collect::<Vec<_>>(),"overs":stats.overs,"non_finite_samples":stats.non_finite_samples,
            "clipping":{"samples_seen":c.echantillons_vus,"clipped_samples":c.echantillons_ecretes,"max_excess_lsb":c.exces_max_lsb,"max_peak":c.crete_max,"first_clip":c.premier_ecretage_a}})
    }
    fn reset(&mut self, _: ResetReason) {
        self.engine.reset_history();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}
