//! Balayage sinusoïdal logarithmique (ESS) et son filtre inverse, d'après
//! Farina, « Simultaneous measurement of impulse response and distortion
//! with a swept-sine technique », AES 108th Convention, 2000, et Farina,
//! « Advancements in impulse response measurements by sine sweeps », AES
//! 122nd Convention, 2007.
//!
//! Le balayage x(t) = sin( K·(exp(t/L) − 1) ) avec K = 2π·f1·L et
//! L = T / ln(f2/f1). Son filtre inverse est le balayage retourné dans le
//! temps, pondéré par une enveloppe exp(−t/L) (−6 dB/octave) qui compense
//! la densité spectrale rose du balayage. La convolution x ⊛ inv est une
//! impulsion ; les produits de distorsion harmonique d'ordre k se replient
//! à t_k = −L·ln(k) AVANT l'impulsion linéaire, d'où leur séparation.

use crate::fft;

/// Paramètres d'un balayage.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepSpec {
    pub sample_rate: u32,
    /// Fréquence de départ (Hz), typiquement 10 à 20 Hz.
    pub f_start: f64,
    /// Fréquence d'arrivée (Hz), typiquement 20 kHz ou fs/2 × 0,9.
    pub f_end: f64,
    /// Durée du balayage (s). 5 à 10 s pour une pièce ; plus long = meilleur RSB.
    pub duration_s: f64,
    /// Amplitude crête linéaire (0 < a ≤ 1). Le garde-fou de niveau la fixe.
    pub amplitude: f64,
    /// Fondu d'entrée, en OCTAVES parcourues par le balayage (1,0 = de f₁ à
    /// 2·f₁). Exprimé ainsi, il adoucit le bord spectral à f₁ de la même
    /// façon quelle que soit la durée ; en secondes il vaut L·octaves·ln 2.
    pub fade_in_octaves: f64,
    /// Fondu de sortie (s).
    pub fade_out_s: f64,
}

impl SweepSpec {
    /// Réglage par défaut d'une mesure de pièce : 10 Hz → 22 kHz (bornes
    /// au-delà de la bande utile 20 Hz – 20 kHz, pour que le ringing des
    /// extrémités du balayage tombe hors bande) en 8 s, −12 dBFS, fondu
    /// d'entrée d'une octave (10 → 20 Hz, soit 0,7 s à 8 s de balayage : adoucit
    /// le bord spectral à 10 Hz, l'ondulation de Gibbs sous 100 Hz passe de
    /// 0,35 dB à moins de 0,2 dB).
    pub fn room_default(sample_rate: u32) -> Self {
        let nyq = sample_rate as f64 / 2.0;
        Self {
            sample_rate,
            f_start: 10.0,
            f_end: (22_000.0f64).min(nyq * 0.9),
            duration_s: 8.0,
            amplitude: fft::from_db(-12.0),
            fade_in_octaves: 1.0,
            fade_out_s: 0.01,
        }
    }

    /// Constante de temps L = T / ln(f2/f1).
    pub fn l_constant(&self) -> f64 {
        self.duration_s / (self.f_end / self.f_start).ln()
    }

    pub fn len_samples(&self) -> usize {
        (self.duration_s * self.sample_rate as f64).round() as usize
    }

    /// Instant (en échantillons, négatif) où se replie l'harmonique d'ordre
    /// `k` dans la réponse déconvoluée, par rapport à l'impulsion linéaire.
    pub fn harmonic_offset_samples(&self, k: u32) -> f64 {
        -self.l_constant() * (k as f64).ln() * self.sample_rate as f64
    }
}

/// Balayage généré et son filtre inverse normalisé.
#[derive(Debug, Clone)]
pub struct Sweep {
    pub spec: SweepSpec,
    /// Signal à jouer, amplitude crête = `spec.amplitude`.
    pub signal: Vec<f64>,
    /// Filtre inverse : `signal ⊛ inverse` ≈ δ de gain unité en bande au rang `signal.len() − 1`.
    pub inverse: Vec<f64>,
}

impl Sweep {
    /// Indice, dans la convolution complète `capture ⊛ inverse`, où tombe
    /// l'impulsion linéaire d'un système sans retard.
    pub fn zero_lag_index(&self) -> usize {
        self.signal.len() - 1
    }
}

/// Génère le balayage et son filtre inverse.
pub fn generate(spec: &SweepSpec) -> Sweep {
    assert!(spec.f_start > 0.0 && spec.f_end > spec.f_start, "bornes du balayage invalides");
    assert!(spec.duration_s > 0.0 && spec.sample_rate > 0);
    let fs = spec.sample_rate as f64;
    let n = spec.len_samples();
    let l = spec.l_constant();
    let k = 2.0 * std::f64::consts::PI * spec.f_start * l;

    let fade_in = (l * spec.fade_in_octaves * std::f64::consts::LN_2 * fs).round() as usize;
    let fade_out = (spec.fade_out_s * fs).round() as usize;

    let mut signal = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / fs;
        let mut v = (k * ((t / l).exp() - 1.0)).sin();
        if fade_in > 0 && i < fade_in {
            // Demi-Hann montant.
            let w = 0.5 - 0.5 * (std::f64::consts::PI * i as f64 / fade_in as f64).cos();
            v *= w;
        }
        if fade_out > 0 && i >= n.saturating_sub(fade_out) {
            let j = n - 1 - i;
            let w = 0.5 - 0.5 * (std::f64::consts::PI * j as f64 / fade_out as f64).cos();
            v *= w;
        }
        signal.push(v * spec.amplitude);
    }

    // Filtre inverse : retournement temporel + enveloppe exp(−t/L).
    let mut inverse: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let env = (-t / l).exp();
            signal[n - 1 - i] * env
        })
        .collect();

    // Normalisation numérique : gain unité DANS LA BANDE. On ne normalise pas
    // par la crête temporelle de l'auto-déconvolution : une impulsion limitée
    // à [f1, f2] a une crête égale au gain en bande multiplié par la fraction
    // de bande occupée (21,6/24 kHz = 0,9 → +1 dB d'erreur sinon).
    let auto = fft::convolve(&signal, &inverse);
    let nfft = fft::next_pow2(auto.len());
    let spec_auto = fft::rfft(&auto, nfft);
    let df = fs / nfft as f64;
    let (lo, hi) = (2.0 * spec.f_start, spec.f_end / 2.0);
    let mut acc = 0.0;
    let mut cnt = 0usize;
    for (i, c) in spec_auto.iter().enumerate() {
        let f = i as f64 * df;
        if f >= lo && f <= hi {
            acc += c.norm();
            cnt += 1;
        }
    }
    let gain = if cnt > 0 { acc / cnt as f64 } else { auto[n - 1].abs() }.max(1e-30);
    inverse.iter_mut().for_each(|v| *v /= gain);

    Sweep { spec: spec.clone(), signal, inverse }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amplitude_crete_respectee() {
        let spec = SweepSpec { amplitude: 0.25, ..SweepSpec::room_default(48_000) };
        let s = generate(&spec);
        let peak = s.signal.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak <= 0.25 + 1e-12 && peak > 0.24, "crête {peak}");
    }

    #[test]
    fn auto_deconvolution_est_une_impulsion() {
        let spec = SweepSpec { duration_s: 2.0, ..SweepSpec::room_default(48_000) };
        let s = generate(&spec);
        let auto = fft::convolve(&s.signal, &s.inverse);
        let z = s.zero_lag_index();
        // Crête = gain en bande (1) × fraction de bande occupée (≈ 21,6/24).
        let fraction = (s.spec.f_end - s.spec.f_start) / 24_000.0;
        assert!((auto[z] - fraction).abs() < 0.03, "crête {} ≠ {fraction}", auto[z]);
        // Gain en bande : magnitude spectrale de l'auto-déconvolution ≈ 0 dB.
        let sp = fft::rfft(&auto, fft::next_pow2(auto.len()));
        let df = 48_000.0 / fft::next_pow2(auto.len()) as f64;
        for (f, tol) in [(50.0, 0.3), (200.0, 0.1), (1000.0, 0.1), (5000.0, 0.1), (15_000.0, 0.1)] {
            let i = (f / df).round() as usize;
            assert!(fft::to_db(sp[i].norm()).abs() < tol, "{f} Hz : {} dB", fft::to_db(sp[i].norm()));
        }
        // Hors de ±2 ms autour de l'impulsion, le résidu reste sous −40 dB.
        let guard = 96;
        let residual = auto
            .iter()
            .enumerate()
            .filter(|(i, _)| (*i as isize - z as isize).unsigned_abs() > guard)
            .fold(0.0f64, |m, (_, v)| m.max(v.abs()));
        assert!(fft::to_db(residual) < -40.0, "résidu {} dB", fft::to_db(residual));
    }

    #[test]
    fn offset_des_harmoniques_negatif_et_croissant() {
        let spec = SweepSpec::room_default(48_000);
        let h2 = spec.harmonic_offset_samples(2);
        let h3 = spec.harmonic_offset_samples(3);
        assert!(h2 < 0.0 && h3 < h2);
    }
}
