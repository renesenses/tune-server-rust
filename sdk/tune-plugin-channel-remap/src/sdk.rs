//! Le processeur du SDK (ABI 1) : sur place, donc N → N seulement.
use crate::{ChannelRemapSettings, Matrice};
use serde::Deserialize;
use tune_plugin_sdk::{Error, Settings, audio::*};

fn reglage(s: &Settings) -> Result<(ChannelRemapSettings, Matrice), Error> {
    let s = ChannelRemapSettings::deserialize(s).map_err(|_| Error::InvalidSettings)?;
    let m = Matrice::depuis_reglage(&s).map_err(|_| Error::InvalidSettings)?;
    Ok((s, m))
}

/// La fabrique du greffon « Réaffectation des canaux ».
pub struct ChannelRemap;

impl DspFactory for ChannelRemap {
    fn assess(&self, ctx: &PlaybackContext, s: &Settings) -> Result<Applicability, Error> {
        if let Some(reason) = policy_bypass(ctx, true) {
            return Ok(Applicability::Bypass(reason));
        }
        let (s, m) = reglage(s)?;
        Ok(if !s.enabled {
            Applicability::Bypass(BypassReason::Disabled)
        } else if m.est_identite() {
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
        let (_, m) = reglage(s)?;
        // Un `AudioBlock` garde son format : le processeur ne peut pas changer
        // le nombre de canaux. N → M est fait par l'hôte, avec la même
        // `Matrice`, à l'adaptation source → périphérique.
        if m.entrees() != m.sorties()
            || format.channels() != m.entrees()
            || format.encoding() == SampleEncoding::F64
        {
            return Err(Error::UnsupportedFormat);
        }
        if max_frames == 0 || max_frames > 4 * 1024 * 1024 {
            return Err(Error::BlockTooLarge);
        }
        let n = usize::from(m.entrees());
        Ok(Box::new(Instance {
            matrice: m,
            format,
            max_frames,
            trame_f32: vec![0.0; n],
            trame_i32: vec![0; n],
            trame_i16: vec![0; n],
            trame_s24: vec![[0; 3]; n],
        }))
    }
}

struct Instance {
    matrice: Matrice,
    format: AudioFormat,
    max_frames: usize,
    trame_f32: Vec<f32>,
    trame_i32: Vec<i32>,
    trame_i16: Vec<i16>,
    trame_s24: Vec<[u8; 3]>,
}

fn s24(b: [u8; 3]) -> f64 {
    f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
}
fn vers_s24(v: f64) -> [u8; 3] {
    let x = v.round().clamp(-8_388_608.0, 8_388_607.0) as i32;
    let b = x.to_le_bytes();
    [b[0], b[1], b[2]]
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
        if self.matrice.est_identite() {
            return Ok(ProcessReport::default());
        }
        let n = usize::from(self.matrice.entrees());
        let m = &self.matrice;
        match block.samples_mut() {
            SamplesMut::F32(s) => {
                if s.iter().any(|x| !x.is_finite()) {
                    return Err(Error::NonFinite);
                }
                for trame in s.chunks_exact_mut(n) {
                    self.trame_f32.copy_from_slice(trame);
                    m.appliquer_trame(&self.trame_f32, trame, 0.0, f64::from, |v| v as f32);
                }
            }
            SamplesMut::S16(s) => {
                for trame in s.chunks_exact_mut(n) {
                    self.trame_i16.copy_from_slice(trame);
                    m.appliquer_trame(&self.trame_i16, trame, 0, f64::from, |v| {
                        v.round().clamp(-32_768.0, 32_767.0) as i16
                    });
                }
            }
            SamplesMut::S32(s) => {
                for trame in s.chunks_exact_mut(n) {
                    self.trame_i32.copy_from_slice(trame);
                    m.appliquer_trame(&self.trame_i32, trame, 0, f64::from, |v| {
                        v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
                    });
                }
            }
            SamplesMut::S24Le(s) => {
                for trame in s.chunks_exact_mut(3 * n) {
                    let (echantillons, _) = trame.as_chunks_mut::<3>();
                    self.trame_s24.copy_from_slice(echantillons);
                    m.appliquer_trame(&self.trame_s24, echantillons, [0; 3], s24, vers_s24);
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
        let (_, m) = reglage(value)?;
        if m.entrees() != self.matrice.entrees() || m.sorties() != self.matrice.sorties() {
            return Err(Error::InvalidFormat);
        }
        self.matrice = m;
        Ok(())
    }

    fn inherit_from(&mut self, previous: &dyn Processor) -> Result<(), Error> {
        // Sans mémoire : rien à transférer, seulement à vérifier.
        let previous = (previous as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .ok_or(Error::InvalidState)?;
        if previous.format != self.format {
            return Err(Error::InvalidFormat);
        }
        Ok(())
    }

    fn diagnostics(&self) -> Settings {
        serde_json::json!({
            "inputs": self.matrice.entrees(),
            "outputs": self.matrice.sorties(),
            "bit_exact_copy": self.matrice.est_recopie(),
            "identity": self.matrice.est_identite(),
            "normalization_db": self.matrice.attenuation_db(),
        })
    }

    fn reset(&mut self, _: ResetReason) {}

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
