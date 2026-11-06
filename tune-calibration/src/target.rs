//! Courbes cibles : plate avec pente réglable, ou courbe de l'utilisateur
//! (points fréquence/dB, par exemple la cible Harman « in-room » de Olive,
//! Welti et McMullin, AES 2013, ou la « B&K 1974 » de Brüel & Kjær).
//!
//! La cible est relative : son niveau absolu est aligné sur la mesure par
//! `align_offset` avant l'ajustement, de sorte que l'égaliseur n'ait que
//! des pics à couper.

use crate::analysis::{interp_log, LogResponse};

#[derive(Debug, Clone, PartialEq)]
pub enum TargetCurve {
    /// Droite en dB/octave, 0 dB au pivot. Pente typique −0,5 à −1 dB/oct ;
    /// 0 = plate.
    Tilt { db_per_octave: f64, pivot_hz: f64 },
    /// Points (Hz, dB) croissants en fréquence, interpolés en log(f).
    Points(Vec<(f64, f64)>),
}

impl TargetCurve {
    pub fn flat() -> Self {
        Self::Tilt {
            db_per_octave: 0.0,
            pivot_hz: 1000.0,
        }
    }

    /// Approximation de la cible Harman pour enceintes en pièce (Olive 2013) :
    /// +~5 dB à 20 Hz, pente douce vers −~5 dB à 20 kHz.
    pub fn harman_in_room() -> Self {
        Self::Points(vec![
            (20.0, 5.5),
            (50.0, 4.5),
            (100.0, 3.0),
            (200.0, 1.5),
            (500.0, 0.0),
            (1000.0, -0.5),
            (2000.0, -1.5),
            (5000.0, -3.0),
            (10000.0, -4.5),
            (20000.0, -6.0),
        ])
    }

    /// Niveau cible (dB, relatif) à la fréquence `f`.
    pub fn level_db(&self, f: f64) -> f64 {
        match self {
            TargetCurve::Tilt {
                db_per_octave,
                pivot_hz,
            } => db_per_octave * (f / pivot_hz).log2(),
            TargetCurve::Points(pts) => {
                let (fr, va): (Vec<f64>, Vec<f64>) = pts.iter().cloned().unzip();
                interp_log(&fr, &va, f)
            }
        }
    }

    /// Cible évaluée sur la grille de `measured`, décalée de `offset_db`.
    pub fn on_grid(&self, freqs: &[f64], offset_db: f64) -> LogResponse {
        LogResponse {
            freqs: freqs.to_vec(),
            magnitude_db: freqs
                .iter()
                .map(|f| self.level_db(*f) + offset_db)
                .collect(),
        }
    }

    /// Décalage (dB) à ajouter à la cible pour qu'elle épouse la mesure sur
    /// la bande de référence `[f_lo, f_hi]` : MÉDIANE des écarts, robuste
    /// aux pics (une moyenne serait tirée vers le haut par les modes et
    /// l'égaliseur, qui ne fait que couper, sous-corrigerait). La cible passe
    /// ainsi par le milieu de la mesure : les pics au-dessus sont coupés, les
    /// creux en dessous restent.
    pub fn align_offset(&self, measured: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
        self.align_offset_percentile(measured, f_lo, f_hi, 0.5)
    }

    /// Variante : la cible passe par le percentile `p` (0,5 = médiane) des
    /// écarts. Un percentile plus bas (0,3) place la cible près du plancher
    /// entre les modes : les coupes sont alors plus profondes et plus larges,
    /// proches des modes eux-mêmes, ce qui raccourcit leur résonance au lieu
    /// de n'en raser que le sommet ; en contrepartie le niveau global baisse.
    pub fn align_offset_percentile(
        &self,
        measured: &LogResponse,
        f_lo: f64,
        f_hi: f64,
        p: f64,
    ) -> f64 {
        let mut d: Vec<f64> = measured
            .freqs
            .iter()
            .zip(&measured.magnitude_db)
            .filter(|(f, _)| **f >= f_lo && **f <= f_hi)
            .map(|(f, m)| m - self.level_db(*f))
            .collect();
        if d.is_empty() {
            return 0.0;
        }
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pos = (p.clamp(0.0, 1.0) * (d.len() - 1) as f64).round() as usize;
        d[pos]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pente_par_octave() {
        let t = TargetCurve::Tilt {
            db_per_octave: -1.0,
            pivot_hz: 1000.0,
        };
        assert!((t.level_db(1000.0)).abs() < 1e-12);
        assert!((t.level_db(4000.0) + 2.0).abs() < 1e-12);
        assert!((t.level_db(250.0) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn alignement_sur_la_mesure() {
        let m = LogResponse {
            freqs: vec![100.0, 200.0, 400.0, 800.0],
            magnitude_db: vec![70.0, 72.0, 71.0, 90.0],
        };
        let off = TargetCurve::flat().align_offset(&m, 50.0, 1000.0);
        // Médiane (arrondie à l'élément supérieur) de {70, 71, 72, 90} = 72 : le pic à 90 dB ne tire pas la cible.
        assert!((off - 72.0).abs() < 1e-12, "{off}");
        let low = TargetCurve::flat().align_offset_percentile(&m, 50.0, 1000.0, 0.0);
        assert!((low - 70.0).abs() < 1e-12);
    }
}
