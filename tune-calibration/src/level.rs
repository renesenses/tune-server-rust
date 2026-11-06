//! Garde-fous de niveau : montée progressive du balayage, plafond de
//! pression acoustique, détection d'écrêtage, rapport signal/bruit minimal.
//!
//! Principe : on ne joue jamais un balayage plein niveau d'emblée. Une
//! première salve courte à bas niveau mesure la crête captée ; on monte par
//! pas de 6 dB tant que la crête captée reste sous `max_capture_dbfs`, que le
//! SPL estimé (si le micro a un facteur de sensibilité) reste sous
//! `max_spl_db`, et que le niveau joué reste sous `max_play_dbfs`. On
//! s'arrête dès que le RSB visé est atteint.

use crate::fft;

#[derive(Debug, Clone, PartialEq)]
pub struct LevelPolicy {
    /// Niveau de départ du balayage (dBFS crête).
    pub start_dbfs: f64,
    /// Pas de montée (dB).
    pub step_db: f64,
    /// Niveau joué maximal (dBFS crête) : laisse une marge au DAC et à l'ampli.
    pub max_play_dbfs: f64,
    /// Crête captée maximale (dBFS) : au-delà, le micro ou l'ADC écrête.
    pub max_capture_dbfs: f64,
    /// Pression acoustique maximale (dB SPL) si connue par le micro.
    pub max_spl_db: f64,
    /// RSB visé sur la réponse impulsionnelle (dB).
    pub target_snr_db: f64,
}

impl Default for LevelPolicy {
    fn default() -> Self {
        Self {
            start_dbfs: -30.0,
            step_db: 6.0,
            max_play_dbfs: -6.0,
            max_capture_dbfs: -6.0,
            max_spl_db: 90.0,
            target_snr_db: 50.0,
        }
    }
}

/// Mesures faites sur une salve d'essai à un niveau donné.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub played_dbfs: f64,
    pub captured_peak_dbfs: f64,
    pub snr_db: f64,
    pub spl_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LevelDecision {
    /// Niveau retenu pour la mesure complète.
    Accept { play_dbfs: f64 },
    /// Monter d'un pas et resonder.
    StepUp { next_play_dbfs: f64 },
    /// Impossible d'atteindre le RSB visé sans dépasser un plafond.
    Refuse { reason: String, best_play_dbfs: f64 },
}

impl LevelPolicy {
    /// Décision après une salve d'essai.
    pub fn decide(&self, probe: &ProbeResult) -> LevelDecision {
        let clipped = probe.captured_peak_dbfs >= -0.1;
        if clipped {
            return LevelDecision::Refuse {
                reason: "écrêtage à la capture : baisser le gain du micro ou éloigner le micro"
                    .into(),
                best_play_dbfs: probe.played_dbfs - self.step_db,
            };
        }
        if probe.snr_db >= self.target_snr_db {
            return LevelDecision::Accept {
                play_dbfs: probe.played_dbfs,
            };
        }
        let next = probe.played_dbfs + self.step_db;
        let gain_room_capture = self.max_capture_dbfs - probe.captured_peak_dbfs;
        let gain_room_spl = probe
            .spl_db
            .map(|s| self.max_spl_db - s)
            .unwrap_or(f64::INFINITY);
        if next > self.max_play_dbfs + 1e-9 {
            return LevelDecision::Refuse {
                reason: format!(
                    "niveau joué plafonné à {:.0} dBFS, RSB {:.0} dB insuffisant",
                    self.max_play_dbfs, probe.snr_db
                ),
                best_play_dbfs: probe.played_dbfs,
            };
        }
        if self.step_db > gain_room_capture {
            return LevelDecision::Refuse {
                reason: format!(
                    "crête captée {:.1} dBFS trop proche du plafond",
                    probe.captured_peak_dbfs
                ),
                best_play_dbfs: probe.played_dbfs,
            };
        }
        if self.step_db > gain_room_spl {
            return LevelDecision::Refuse {
                reason: format!(
                    "pression acoustique {:.0} dB SPL proche du plafond {:.0}",
                    probe.spl_db.unwrap_or(0.0),
                    self.max_spl_db
                ),
                best_play_dbfs: probe.played_dbfs,
            };
        }
        LevelDecision::StepUp {
            next_play_dbfs: next,
        }
    }
}

/// Crête (dBFS) d'un signal.
pub fn peak_dbfs(x: &[f64]) -> f64 {
    fft::to_db(x.iter().fold(0.0f64, |m, v| m.max(v.abs())))
}

/// Niveau efficace (dBFS) d'un signal.
pub fn rms_dbfs(x: &[f64]) -> f64 {
    if x.is_empty() {
        return f64::NEG_INFINITY;
    }
    fft::to_db((x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt())
}

/// Nombre d'échantillons écrêtés (|x| ≥ seuil).
pub fn clipped_samples(x: &[f64], threshold: f64) -> usize {
    x.iter().filter(|v| v.abs() >= threshold).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monte_puis_accepte() {
        let p = LevelPolicy::default();
        let d = p.decide(&ProbeResult {
            played_dbfs: -30.0,
            captured_peak_dbfs: -40.0,
            snr_db: 30.0,
            spl_db: None,
        });
        assert_eq!(
            d,
            LevelDecision::StepUp {
                next_play_dbfs: -24.0
            }
        );
        let d = p.decide(&ProbeResult {
            played_dbfs: -24.0,
            captured_peak_dbfs: -34.0,
            snr_db: 55.0,
            spl_db: None,
        });
        assert_eq!(d, LevelDecision::Accept { play_dbfs: -24.0 });
    }

    #[test]
    fn refuse_au_plafond_spl() {
        let p = LevelPolicy::default();
        let d = p.decide(&ProbeResult {
            played_dbfs: -24.0,
            captured_peak_dbfs: -30.0,
            snr_db: 20.0,
            spl_db: Some(88.0),
        });
        assert!(matches!(d, LevelDecision::Refuse { .. }), "{d:?}");
    }

    #[test]
    fn refuse_au_plafond_joue() {
        let p = LevelPolicy::default();
        let d = p.decide(&ProbeResult {
            played_dbfs: -6.0,
            captured_peak_dbfs: -40.0,
            snr_db: 20.0,
            spl_db: None,
        });
        assert!(matches!(d, LevelDecision::Refuse { .. }));
    }

    #[test]
    fn ecretage_detecte() {
        assert_eq!(clipped_samples(&[0.5, 1.0, -0.999, 0.2], 0.99), 2);
    }
}
