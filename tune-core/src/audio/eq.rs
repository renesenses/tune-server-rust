//! Parametric equalizer host facade. Active EQ modifies samples; f64 arithmetic
//! does not imply bit-perfection. Both bundled and signed native providers use
//! the same persisted profile and producer locations (including radio).
use super::ecretage::{CompteurDEcretage, EtageEcretant, Portee, REGISTRE, dire_fin, dire_premier};
use std::sync::atomic::{AtomicBool, Ordering};
pub use tune_plugin_equalizer::{
    DEBIT_DE_REFERENCE_HZ, EqBandSpec, EqProcessStats, EqProfile, HeadroomMode, ListeningMode,
    RoomSize, SpeakerPlacement,
};
use tune_plugin_native::stage::Stage;
pub struct EqProcessor {
    engine: Engine,
    /// #4685 — niveau moyen du filtre, réserve comprise, calculé UNE fois à
    /// la construction (voir [`Self::gain_moyen_db`]).
    gain_moyen_db: f64,
    clipping: CompteurDEcretage,
    closed: AtomicBool,
}
enum Engine {
    Bundled(tune_plugin_equalizer::EqProcessor),
    Native(Stage),
    Unavailable,
}
impl EqProcessor {
    pub fn new(profile: &EqProfile, sample_rate: u32, channels: u16) -> Self {
        let engine = if tune_plugin_native::failure("equalizer").is_some() {
            Engine::Unavailable
        } else if let Some(provider) = tune_plugin_native::provider("equalizer") {
            match serde_json::to_value(profile)
                .map_err(|e| e.to_string())
                .and_then(|settings| {
                    Stage::prepare(provider, sample_rate, channels, &settings)
                        .map_err(|e| e.to_string())
                }) {
                Ok(stage) => Engine::Native(stage),
                Err(error) => {
                    tracing::error!(%error,"native_equalizer_prepare_failed");
                    Engine::Unavailable
                }
            }
        } else {
            Engine::Bundled(tune_plugin_equalizer::EqProcessor::new(
                profile,
                sample_rate,
                channels,
            ))
        };
        // Calculé depuis le PROFIL, pour les deux moteurs : le greffon natif
        // signé exécute la même arithmétique que le moteur embarqué (même
        // crate), et ne publie pas d'autre porte. Un moteur indisponible ne
        // filtre rien — il n'a donc rien à compenser.
        let gain_moyen_db = if matches!(engine, Engine::Unavailable) {
            0.0
        } else {
            profile.gain_moyen_db_at(channels, f64::from(sample_rate))
        };
        Self {
            engine,
            gain_moyen_db,
            clipping: Default::default(),
            closed: AtomicBool::new(false),
        }
    }

    /// #4685 — ce que cet égaliseur, réserve automatique comprise, fait
    /// gagner ou perdre au niveau MOYEN, en dB, sur un bruit rose. Négatif
    /// dès qu'une bande pousse : la réserve anti-écrêtage retire plus que la
    /// courbe ne rend en moyenne. Voir `EqProfile::gain_moyen_db_at`.
    pub fn gain_moyen_db(&self) -> f64 {
        self.gain_moyen_db
    }
    pub fn process_pcm(&mut self, pcm: &mut [u8], depth: u16) -> EqProcessStats {
        match &mut self.engine {
            Engine::Bundled(p) => p.process_pcm(pcm, depth),
            Engine::Native(p) => record_native(&mut self.clipping, p.process_pcm(pcm, depth)),
            Engine::Unavailable => EqProcessStats::default(),
        }
    }
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> EqProcessStats {
        match &mut self.engine {
            Engine::Bundled(p) => p.process_interleaved(samples),
            Engine::Native(p) => record_native(&mut self.clipping, p.process_f32(samples)),
            Engine::Unavailable => EqProcessStats::default(),
        }
    }
    pub fn is_enabled(&self) -> bool {
        match &self.engine {
            Engine::Bundled(p) => p.is_enabled(),
            Engine::Native(p) => p.info["enabled"].as_bool().unwrap_or(false),
            Engine::Unavailable => false,
        }
    }
    pub fn response(&self, sample_rate: u32) -> serde_json::Value {
        match &self.engine {
            Engine::Bundled(p) => p.response(sample_rate),
            Engine::Native(p) => p.info["response"].clone(),
            Engine::Unavailable => serde_json::Value::Null,
        }
    }
    pub fn preamp_db(&self, channel: u16) -> Option<f64> {
        match &self.engine {
            Engine::Bundled(p) => p.preamp_db(channel),
            Engine::Native(p) => p.info["preamp_db"].get(channel as usize)?.as_f64(),
            Engine::Unavailable => None,
        }
    }
    pub fn process_stats(&self) -> EqProcessStats {
        match &self.engine {
            Engine::Bundled(p) => p.process_stats(),
            Engine::Native(p) => EqProcessStats {
                overs: p.report.clipped_samples,
                non_finite_samples: p.report.non_finite_samples,
            },
            Engine::Unavailable => EqProcessStats::default(),
        }
    }
    pub fn ecretage(&self) -> super::ecretage::CompteurDEcretage {
        match &self.engine {
            Engine::Bundled(p) => p.ecretage(),
            Engine::Native(_) => self.clipping,
            Engine::Unavailable => Default::default(),
        }
    }
    pub fn inherit_state_from(&mut self, previous: &Self) {
        match (&mut self.engine, &previous.engine) {
            (Engine::Bundled(new), Engine::Bundled(old)) => new.inherit_state_from(old),
            (Engine::Native(new), Engine::Native(old)) => {
                if let Err(error) = new.inherit(old) {
                    tracing::debug!(%error,"native_eq_history_not_compatible");
                } else {
                    self.clipping = previous.clipping;
                    previous.closed.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }
}
impl Drop for EqProcessor {
    fn drop(&mut self) {
        if matches!(self.engine, Engine::Native(_))
            && !self.closed.swap(true, Ordering::Relaxed)
            && self.clipping.echantillons_ecretes > 0
        {
            REGISTRE.egaliseur.piste_close();
            dire_fin(EtageEcretant::Egaliseur, Portee::Piste, &self.clipping);
        }
    }
}
fn record_native(
    previous: &mut CompteurDEcretage,
    result: Result<tune_plugin_sdk::audio::ProcessReport, tune_plugin_sdk::Error>,
) -> EqProcessStats {
    match result {
        Ok(report) => {
            if let Some(c) = report.clipping {
                let next = CompteurDEcretage {
                    echantillons_vus: c.samples_seen,
                    echantillons_ecretes: c.clipped_samples,
                    exces_max_lsb: c.max_excess_lsb,
                    crete_max: f64::from_bits(c.max_peak_bits),
                    premier_ecretage_a: c.first_clip,
                };
                REGISTRE.egaliseur.absorber(previous, &next);
                if previous.echantillons_ecretes == 0 && next.echantillons_ecretes > 0 {
                    dire_premier(EtageEcretant::Egaliseur, Portee::Piste, &next);
                }
                *previous = next;
            }
            EqProcessStats {
                overs: report.clipped_samples,
                non_finite_samples: report.non_finite_samples,
            }
        }
        Err(error) => {
            tracing::error!(%error,"native_equalizer_processing_failed");
            EqProcessStats::default()
        }
    }
}
