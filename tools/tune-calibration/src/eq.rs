//! Filtres biquad du « Cookbook » de R. Bristow-Johnson (mêmes formules que
//! `sdk/tune-plugin-equalizer/src/engine.rs`) et bandes au format de
//! l'égaliseur natif de Tune (`{"freq", "gain", "q", "type"}`). En C1 ils
//! servent à la simulation (modes de pièce, coloration de micro) et à
//! l'esquisse FIR ; l'ajustement automatique (`fit`) arrive avec C2.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Type de filtre, limité à ce que l'égaliseur natif accepte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandType {
    Peak,
    LowShelf,
    HighShelf,
}

/// Une bande au format natif (`sdk/tune-plugin-equalizer`, `EqBandSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
    #[serde(rename = "type")]
    pub kind: BandType,
    /// 0 = gauche, 1 = droite, absent = tous les canaux.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel: Option<u16>,
}

/// Coefficients biquad normalisés (a0 = 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// Peaking EQ (RBJ), Q en Q « classique ».
    pub fn peaking(fs: f64, f0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * w0.cos()) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * w0.cos()) / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    /// Low shelf (RBJ, pente S = 1 comme l'égaliseur natif).
    pub fn low_shelf(fs: f64, f0: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / 2.0 * 2f64.sqrt();
        let (c, sa) = (w0.cos(), 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) + (a - 1.0) * c + sa;
        Self {
            b0: a * ((a + 1.0) - (a - 1.0) * c + sa) / a0,
            b1: 2.0 * a * ((a - 1.0) - (a + 1.0) * c) / a0,
            b2: a * ((a + 1.0) - (a - 1.0) * c - sa) / a0,
            a1: -2.0 * ((a - 1.0) + (a + 1.0) * c) / a0,
            a2: ((a + 1.0) + (a - 1.0) * c - sa) / a0,
        }
    }

    /// High shelf (RBJ, S = 1).
    pub fn high_shelf(fs: f64, f0: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / 2.0 * 2f64.sqrt();
        let (c, sa) = (w0.cos(), 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * c + sa;
        Self {
            b0: a * ((a + 1.0) + (a - 1.0) * c + sa) / a0,
            b1: -2.0 * a * ((a - 1.0) + (a + 1.0) * c) / a0,
            b2: a * ((a + 1.0) + (a - 1.0) * c - sa) / a0,
            a1: 2.0 * ((a - 1.0) - (a + 1.0) * c) / a0,
            a2: ((a + 1.0) - (a - 1.0) * c - sa) / a0,
        }
    }

    pub fn from_band(fs: f64, b: &Band) -> Self {
        match b.kind {
            BandType::Peak => Self::peaking(fs, b.freq, b.gain, b.q),
            BandType::LowShelf => Self::low_shelf(fs, b.freq, b.gain),
            BandType::HighShelf => Self::high_shelf(fs, b.freq, b.gain),
        }
    }

    /// Module de la réponse (dB) à la fréquence `f`.
    pub fn magnitude_db(&self, fs: f64, f: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        // H(e^{jw}) = (b0 + b1 e^{-jw} + b2 e^{-2jw}) / (1 + a1 e^{-jw} + a2 e^{-2jw})
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let num = num_re * num_re + num_im * num_im;
        let den = den_re * den_re + den_im * den_im;
        10.0 * (num / den.max(1e-300)).max(1e-30).log10()
    }
}

/// Réponse cumulée (dB) d'une liste de bandes sur une grille.
pub fn bands_response_db(fs: f64, bands: &[Band], freqs: &[f64]) -> Vec<f64> {
    let biquads: Vec<Biquad> = bands.iter().map(|b| Biquad::from_band(fs, b)).collect();
    freqs.iter().map(|f| biquads.iter().map(|b| b.magnitude_db(fs, *f)).sum()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaking_a_le_bon_gain_au_centre_et_zero_loin() {
        let b = Biquad::peaking(48_000.0, 100.0, -6.0, 4.0);
        assert!((b.magnitude_db(48_000.0, 100.0) + 6.0).abs() < 1e-6);
        assert!(b.magnitude_db(48_000.0, 5000.0).abs() < 0.05);
    }

    #[test]
    fn serialisation_au_format_natif() {
        let b = Band { freq: 55.5, gain: -4.25, q: 3.0, kind: BandType::Peak, channel: None };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j, serde_json::json!({"freq": 55.5, "gain": -4.25, "q": 3.0, "type": "peak"}));
    }
}
