//! Prototype C1/C2 de la calibration de la pièce d'écoute (issue #7632).
//!
//! Chaîne de mesure : [`sweep`] (balayage de Farina et filtre inverse) →
//! [`deconv`] (réponse impulsionnelle, retard, repère de synchronisation) →
//! [`analysis`] (fenêtrage, réponse en fréquence, lissage, RT60, cascade,
//! moyenne spatiale) → [`mic_cal`] (étalonnage du micro) ; [`eq`] porte les
//! biquads et le format de bande natif (l'ajustement automatique vient avec
//! C2) ; [`fir`] esquisse C3 (FIR phase mixte et coût du convolueur) ;
//! [`rew`] (import/export REW). [`level`] porte les garde-fous de niveau et
//! [`sim`] les pièces simulées des essais.
//!
//! Aucun son n'est joué ni capté : tout est hors serveur, hors temps réel.

pub mod analysis;
pub mod deconv;
pub mod eq;
pub mod fft;
pub mod fir;
pub mod level;
pub mod mic_cal;
pub mod rew;
pub mod sim;
pub mod sweep;

use analysis::{BandRt60, LogResponse};
use deconv::ImpulseResponse;

/// Paramètres d'une mesure complète en un point.
#[derive(Debug, Clone)]
pub struct MeasurementSpec {
    pub sweep: sweep::SweepSpec,
    pub extract: deconv::ExtractSpec,
    /// Fenêtre d'analyse après l'arrivée directe (ms). 500 ms donne une
    /// résolution de 2 Hz, nécessaire pour les modes ; plus court (gating)
    /// isole le son direct.
    pub window_post_ms: f64,
    /// Lissage 1/N octave.
    pub smoothing_n: usize,
    pub grid_lo: f64,
    pub grid_hi: f64,
    pub points_per_octave: usize,
}

impl MeasurementSpec {
    pub fn room_default(sample_rate: u32) -> Self {
        Self {
            sweep: sweep::SweepSpec::room_default(sample_rate),
            extract: deconv::ExtractSpec::default(),
            window_post_ms: 500.0,
            smoothing_n: 12,
            grid_lo: 20.0,
            grid_hi: 20_000.0,
            points_per_octave: 48,
        }
    }

    pub fn grid(&self) -> Vec<f64> {
        LogResponse::grid(self.grid_lo, self.grid_hi.min(self.sweep.sample_rate as f64 * 0.45), self.points_per_octave)
    }
}

/// Résultat d'une mesure en un point : ce que l'écran C1 affiche.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub ir: ImpulseResponse,
    pub response: LogResponse,
    pub rt60: Vec<BandRt60>,
}

/// Déconvolue une capture et en tire réponse et RT60, avec étalonnage micro facultatif.
pub fn analyse_capture(
    captured: &[f64],
    sweep: &sweep::Sweep,
    spec: &MeasurementSpec,
    mic: Option<&mic_cal::MicCalibration>,
) -> Measurement {
    let ir = deconv::deconvolve(captured, sweep, &spec.extract);
    let grid = spec.grid();
    let mut response = analysis::measured_response(&ir, spec.window_post_ms, spec.smoothing_n, &grid);
    if let Some(m) = mic {
        response.magnitude_db = m.correct(&response.freqs, &response.magnitude_db);
    }
    let rt60 = analysis::rt60_by_band(&ir, &analysis::OCTAVE_CENTERS, 1.0);
    Measurement { ir, response, rt60 }
}
