use crate::engine::{
    CrossfeedProEngine, MAX_AMOUNT, MAX_DELAY_MS, MAX_HEAD_SHADOW_HZ, MAX_PHASE_GUARD_MS,
    MIN_AMOUNT, MIN_HEAD_SHADOW_HZ, MIN_PHASE_GUARD_MS, Params, Preset,
};
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::{Error, Settings, audio::*};

/// Réglages de Crossfeed Pro, tels que la zone les enregistre.
///
/// Défauts : éteint ; dosage 0,30 ; retard 0,3 ms ; ombre de la tête
/// DÉSACTIVÉE (700 Hz si on l'allume) ; coupe-bas désactivé ; garde de phase
/// ACTIVE (30 ms) — elle ne fait rien sur un signal dont la corrélation L/R
/// reste positive.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CrossfeedProSettings {
    pub enabled: bool,
    /// Dosage k de la voie croisée, de 0,20 à 0,60.
    #[cfg_attr(feature = "schemas", schemars(range(min = 0.2, max = 0.6)))]
    pub amount: f32,
    /// Retard de la voie croisée, de 0 à 1 ms.
    #[cfg_attr(feature = "schemas", schemars(range(min = 0.0, max = 1.0)))]
    pub delay_ms: f32,
    /// Passe-bas « ombre de la tête » (1er ordre, 6 dB/octave) sur la voie
    /// croisée.
    pub head_shadow: bool,
    /// Sa fréquence de coupure, de 100 Hz à 10 kHz.
    #[cfg_attr(feature = "schemas", schemars(range(min = 100.0, max = 10000.0)))]
    pub head_shadow_hz: f32,
    /// Coupe-bas du 1er ordre à 150 Hz sur la voie croisée.
    pub low_cut: bool,
    /// Garde de phase : baisse le dosage quand la corrélation L/R devient
    /// négative.
    pub phase_guard: bool,
    /// Constante de temps de la corrélation lissée, de 20 à 50 ms.
    #[cfg_attr(feature = "schemas", schemars(range(min = 20.0, max = 50.0)))]
    pub phase_guard_ms: f32,
}

impl Default for CrossfeedProSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            amount: 0.30,
            delay_ms: 0.3,
            head_shadow: false,
            head_shadow_hz: 700.0,
            low_cut: false,
            phase_guard: true,
            phase_guard_ms: 30.0,
        }
    }
}

impl CrossfeedProSettings {
    /// Poser les valeurs d'un préréglage : dosage, retard, ombre de la tête
    /// et sa coupure. Le reste (activation, coupe-bas, garde) n'est pas
    /// touché, et l'utilisateur peut tout retoucher ensuite.
    pub fn apply_preset(&mut self, preset: Preset) {
        self.amount = preset.amount();
        // libbs2b n'a pas de ligne à retard : son passe-bas fait le retard.
        self.delay_ms = 0.0;
        self.head_shadow = true;
        self.head_shadow_hz = preset.cut_hz();
    }

    fn params(&self) -> Params {
        if !self.enabled {
            return Params::IDENTITE;
        }
        Params {
            amount: self.amount,
            delay_ms: self.delay_ms,
            head_shadow_hz: self.head_shadow.then_some(self.head_shadow_hz),
            low_cut: self.low_cut,
            phase_guard_ms: self.phase_guard.then_some(self.phase_guard_ms),
        }
    }
}

/// Les préréglages, pour un client : identifiant, nom, source et réglages
/// complets obtenus depuis les défauts.
pub fn presets() -> Settings {
    Settings::Array(
        Preset::ALL
            .iter()
            .map(|p| {
                let mut s = CrossfeedProSettings::default();
                s.apply_preset(*p);
                serde_json::json!({
                    "id": p.id(),
                    "name": p.name(),
                    "source": p.source(),
                    "cut_hz": p.cut_hz(),
                    "feed_db": p.feed_db(),
                    "settings": s,
                })
            })
            .collect(),
    )
}

fn settings(s: &Settings) -> Result<CrossfeedProSettings, Error> {
    let s = CrossfeedProSettings::deserialize(s).map_err(|_| Error::InvalidSettings)?;
    let dans = |v: f32, min: f32, max: f32| v.is_finite() && (min..=max).contains(&v);
    if !dans(s.amount, MIN_AMOUNT, MAX_AMOUNT)
        || !dans(s.delay_ms, 0.0, MAX_DELAY_MS)
        || !dans(s.head_shadow_hz, MIN_HEAD_SHADOW_HZ, MAX_HEAD_SHADOW_HZ)
        || !dans(s.phase_guard_ms, MIN_PHASE_GUARD_MS, MAX_PHASE_GUARD_MS)
    {
        return Err(Error::InvalidSettings);
    }
    Ok(s)
}

pub struct CrossfeedPro;
impl DspFactory for CrossfeedPro {
    fn assess(&self, ctx: &PlaybackContext, s: &Settings) -> Result<Applicability, Error> {
        if let Some(reason) = policy_bypass(ctx, true) {
            return Ok(Applicability::Bypass(reason));
        }
        let s = settings(s)?;
        Ok(if !s.enabled {
            Applicability::Bypass(BypassReason::Disabled)
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
            engine: CrossfeedProEngine::new(format.sample_rate(), s.params()),
            format,
            max_frames,
            scratch: vec![0.0; count],
        }))
    }
}

struct Instance {
    engine: CrossfeedProEngine,
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
        let identite = self.engine.is_identity();
        let scratch = &mut self.scratch[..block.frames() * 2];
        match block.samples_mut() {
            SamplesMut::F32(s) => {
                if s.iter().any(|x| !x.is_finite()) {
                    return Err(Error::NonFinite);
                }
                if identite {
                    self.engine.observe_interleaved(s);
                } else {
                    self.engine.process_interleaved(s);
                }
            }
            SamplesMut::S16(s) => {
                for (v, x) in scratch.iter_mut().zip(s.iter()) {
                    *v = *x as f32 / 32768.0;
                }
                if identite {
                    self.engine.observe_interleaved(scratch);
                } else {
                    self.engine.process_interleaved(scratch);
                    for (x, v) in s.iter_mut().zip(scratch.iter()) {
                        *x = crate::engine::quantifier_i16(*v);
                    }
                }
            }
            SamplesMut::S24Le(s) => {
                for (v, b) in scratch.iter_mut().zip(s.as_chunks::<3>().0.iter()) {
                    *v = i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / 2147483648.0;
                }
                if identite {
                    self.engine.observe_interleaved(scratch);
                } else {
                    self.engine.process_interleaved(scratch);
                    for (b, v) in s.as_chunks_mut::<3>().0.iter_mut().zip(scratch.iter()) {
                        b.copy_from_slice(&crate::engine::quantifier_i24(*v));
                    }
                }
            }
            SamplesMut::S32(s) => {
                for (v, x) in scratch.iter_mut().zip(s.iter()) {
                    *v = *x as f32 / 2147483648.0;
                }
                if identite {
                    self.engine.observe_interleaved(scratch);
                } else {
                    self.engine.process_interleaved(scratch);
                    for (x, v) in s.iter_mut().zip(scratch.iter()) {
                        *x = crate::engine::quantifier_i32(*v);
                    }
                }
            }
            SamplesMut::F64(_) => return Err(Error::UnsupportedFormat),
        }
        Ok(ProcessReport {
            changed: !identite,
            ..Default::default()
        })
    }
    /// `audio-live-update` : le nouveau réglage passe par un fondu de 20 ms,
    /// jamais par une marche.
    fn update(&mut self, value: &Settings) -> Result<(), Error> {
        let s = settings(value)?;
        self.engine.set_params(s.params());
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
        serde_json::json!({
            "amount": self.engine.params().amount,
            "effective_amount": self.engine.effective_amount(),
            "correlation": self.engine.correlation(),
            "delay_samples": self.engine.delay_samples(),
        })
    }
    fn reset(&mut self, _: ResetReason) {
        self.engine.reset_history();
    }
    fn latency_frames(&self) -> u32 {
        0
    } // voie directe immédiate ; le retard de la voie croisée est un effet
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}
