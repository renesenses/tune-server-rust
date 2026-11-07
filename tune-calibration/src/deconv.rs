//! Déconvolution d'une capture par le filtre inverse du balayage, extraction
//! de la réponse impulsionnelle linéaire et alignement temporel.

use crate::fft;
use crate::sweep::Sweep;

/// Réponse impulsionnelle extraite d'une mesure.
#[derive(Debug, Clone)]
pub struct ImpulseResponse {
    pub sample_rate: u32,
    /// Échantillons, l'arrivée directe se trouvant au rang `peak_index`.
    pub samples: Vec<f64>,
    /// Rang de l'arrivée directe dans `samples`.
    pub peak_index: usize,
    /// Retard total (échantillons) entre le début du signal joué et l'arrivée
    /// directe : latence des périphériques + temps de vol acoustique.
    pub delay_samples: usize,
}

impl ImpulseResponse {
    pub fn delay_ms(&self) -> f64 {
        self.delay_samples as f64 * 1000.0 / self.sample_rate as f64
    }

    /// Amplitude crête de l'arrivée directe.
    pub fn peak_amplitude(&self) -> f64 {
        self.samples[self.peak_index].abs()
    }

    /// Rapport crête/bruit (dB) : amplitude de l'arrivée directe contre le
    /// niveau efficace de ce qui précède l'arrivée (bruit de fond, résidu de
    /// déconvolution, distorsion repliée), hors 2 ms de pré-écho du fenêtrage.
    /// C'est la grandeur « IR peak to noise » de REW.
    pub fn snr_db(&self) -> f64 {
        let fs = self.sample_rate as f64;
        let guard = (0.002 * fs) as usize;
        let noise_end = self.peak_index.saturating_sub(guard);
        if noise_end < 16 {
            return f64::INFINITY;
        }
        let noise_rms = (self.samples[..noise_end].iter().map(|v| v * v).sum::<f64>()
            / noise_end as f64)
            .sqrt();
        20.0 * (self.peak_amplitude() / noise_rms.max(1e-30)).log10()
    }
}

/// Paramètres d'extraction.
#[derive(Debug, Clone)]
pub struct ExtractSpec {
    /// Durée conservée avant l'arrivée directe (s). Doit rester inférieure à
    /// |t_2| (repli de l'harmonique 2 : 0,72 s pour 8 s de balayage) pour
    /// exclure la distorsion, et couvrir la fenêtre gauche de l'analyse (100 ms).
    pub pre_s: f64,
    /// Durée conservée après l'arrivée directe (s).
    pub post_s: f64,
}

impl Default for ExtractSpec {
    fn default() -> Self {
        Self {
            pre_s: 0.15,
            post_s: 1.5,
        }
    }
}

/// Déconvolue `captured` (capture micro, même taux que le balayage) et
/// extrait la réponse impulsionnelle linéaire autour de l'arrivée directe.
///
/// `captured` peut être plus long que le balayage (silence de garde après).
pub fn deconvolve(captured: &[f64], sweep: &Sweep, spec: &ExtractSpec) -> ImpulseResponse {
    let fs = sweep.spec.sample_rate as f64;
    let full = fft::convolve(captured, &sweep.inverse);
    let z = sweep.zero_lag_index();

    // L'arrivée directe est le maximum absolu à partir du retard nul : avant
    // z ne se trouvent que les répliques harmoniques (distorsion).
    let search_start = z.min(full.len().saturating_sub(1));
    let rel_peak = fft::argmax_abs(&full[search_start..]);
    let peak = search_start + rel_peak;

    let pre = (spec.pre_s * fs).round() as usize;
    let post = (spec.post_s * fs).round() as usize;
    let start = peak.saturating_sub(pre);
    let end = (peak + post).min(full.len());
    let samples = full[start..end].to_vec();

    ImpulseResponse {
        sample_rate: sweep.spec.sample_rate,
        peak_index: peak - start,
        delay_samples: peak - z,
        samples,
    }
}

/// Retrouve, par intercorrélation, le décalage (échantillons) d'un repère de
/// synchronisation `marker` connu dans la capture `captured`. Sert quand la
/// lecture et la capture ne partagent pas d'horloge de départ (micro du
/// téléphone, sortie réseau) : le repère précède le balayage et donne t0.
pub fn find_marker_offset(captured: &[f64], marker: &[f64]) -> usize {
    let r = fft::cross_correlate(captured, marker);
    fft::argmax_abs(&r)
}

/// Repère de synchronisation : courte salve de bruit pseudo-aléatoire
/// déterministe (générateur xorshift), à forte autocorrélation.
pub fn sync_marker(sample_rate: u32, duration_s: f64, amplitude: f64) -> Vec<f64> {
    let n = (duration_s * sample_rate as f64).round() as usize;
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let u = (state >> 11) as f64 / (1u64 << 53) as f64; // [0,1)
            (2.0 * u - 1.0) * amplitude
        })
        .collect()
}

/// Estime la dérive d'horloge (ppm) entre lecture et capture à partir de
/// deux repères placés au début et à la fin d'une séquence : si l'écart
/// mesuré entre les repères diffère de l'écart nominal, les horloges ne
/// battent pas au même rythme. Positif = la capture tourne plus vite.
pub fn clock_drift_ppm(nominal_gap_samples: usize, measured_gap_samples: usize) -> f64 {
    (measured_gap_samples as f64 - nominal_gap_samples as f64) / nominal_gap_samples as f64 * 1e6
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::{generate, SweepSpec};

    #[test]
    fn retard_pur_retrouve_au_sample_pres() {
        let spec = SweepSpec {
            duration_s: 2.0,
            ..SweepSpec::room_default(48_000)
        };
        let s = generate(&spec);
        let delay = 777;
        let mut captured = vec![0.0; delay];
        captured.extend(s.signal.iter().map(|v| v * 0.5));
        captured.extend(std::iter::repeat(0.0).take(48_000));
        let ir = deconvolve(&captured, &s, &ExtractSpec::default());
        assert_eq!(ir.delay_samples, delay);
        // Crête = gain (0,5) × fraction de bande occupée par le balayage.
        let fraction = (spec.f_end - spec.f_start) / 24_000.0;
        assert!(
            (ir.peak_amplitude() - 0.5 * fraction).abs() < 0.02,
            "crête {}",
            ir.peak_amplitude()
        );
    }

    #[test]
    fn repere_de_synchronisation_retrouve() {
        let m = sync_marker(48_000, 0.1, 0.5);
        let mut cap = vec![0.0; 10_000];
        cap[4321..4321 + m.len()].copy_from_slice(&m);
        cap.extend(std::iter::repeat(0.0).take(1000));
        assert_eq!(find_marker_offset(&cap, &m), 4321);
    }

    #[test]
    fn derive_horloge() {
        assert!((clock_drift_ppm(1_000_000, 1_000_050) - 50.0).abs() < 1e-9);
    }
}
