//! Pièces simulées pour les essais : réponse impulsionnelle synthétique à
//! modes de pièce connus (résonances paramétriques), réflexions discrètes,
//! queue réverbérante à RT60 connu par bande, retard d'arrivée, bruit de
//! fond et écart de réponse du micro. Aucun son n'est joué ni capté : la
//! « capture » est la convolution du balayage par la réponse simulée.

use crate::eq::{Band, BandType, Biquad};
use crate::fft;

/// Mode de pièce : résonance paramétrique (+gain dB, Q) à la fréquence `freq`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomMode {
    pub freq: f64,
    pub gain_db: f64,
    pub q: f64,
}

/// Queue réverbérante d'une bande d'octave : RT60 en secondes.
#[derive(Debug, Clone, PartialEq)]
pub struct TailBand {
    pub center_hz: f64,
    pub rt60_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoomSim {
    pub sample_rate: u32,
    /// Retard d'arrivée du son direct (s) : latence des périphériques + vol.
    pub direct_delay_s: f64,
    pub direct_gain: f64,
    pub modes: Vec<RoomMode>,
    /// Réflexions discrètes (retard après le son direct en s, gain linéaire signé).
    pub reflections: Vec<(f64, f64)>,
    /// Queue diffuse par bande ; vide = pièce sèche.
    pub tail: Vec<TailBand>,
    /// Niveau de la queue (dB) relatif au son direct, à t = 0.
    pub tail_level_db: f64,
    /// Bruit de fond ajouté à la capture (dBFS efficace), −∞ = aucun.
    pub noise_floor_dbfs: f64,
    /// Durée de la réponse synthétisée (s).
    pub length_s: f64,
}

impl RoomSim {
    /// Pièce sèche : trois modes typiques d'un salon (axial longueur, largeur,
    /// hauteur), une réflexion de sol, 12 ms de latence.
    pub fn dry_living_room(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            direct_delay_s: 0.012,
            direct_gain: 0.3,
            modes: vec![
                RoomMode {
                    freq: 42.0,
                    gain_db: 9.0,
                    q: 6.0,
                },
                RoomMode {
                    freq: 68.0,
                    gain_db: 6.0,
                    q: 5.0,
                },
                RoomMode {
                    freq: 115.0,
                    gain_db: 5.0,
                    q: 7.0,
                },
            ],
            reflections: vec![(0.0035, -0.12)],
            tail: Vec::new(),
            tail_level_db: -30.0,
            noise_floor_dbfs: -80.0,
            length_s: 1.0,
        }
    }

    /// Pièce réverbérante : mêmes modes, queue diffuse avec RT60 par octave
    /// à partir de 125 Hz. Sous la fréquence de Schroeder (~150 Hz ici) le
    /// champ n'est pas diffus : la décroissance y vient des modes eux-mêmes,
    /// pas d'un bruit superposé.
    pub fn reverberant_living_room(sample_rate: u32) -> Self {
        Self {
            tail: vec![
                TailBand {
                    center_hz: 125.0,
                    rt60_s: 0.7,
                },
                TailBand {
                    center_hz: 250.0,
                    rt60_s: 0.55,
                },
                TailBand {
                    center_hz: 500.0,
                    rt60_s: 0.45,
                },
                TailBand {
                    center_hz: 1000.0,
                    rt60_s: 0.40,
                },
                TailBand {
                    center_hz: 2000.0,
                    rt60_s: 0.35,
                },
                TailBand {
                    center_hz: 4000.0,
                    rt60_s: 0.30,
                },
            ],
            // −30 dB crête à t = 0 : avec une décroissance de 0,7 s, le niveau
            // stationnaire de la queue dépasse le son direct d'environ 4 dB
            // (ce qu'on mesure à 3 m dans un salon vivant).
            tail_level_db: -30.0,
            length_s: 1.5,
            ..Self::dry_living_room(sample_rate)
        }
    }

    /// Réponse impulsionnelle de la pièce (sans retard : l'arrivée directe est à 0).
    pub fn impulse_response(&self) -> Vec<f64> {
        let fs = self.sample_rate as f64;
        let n = (self.length_s * fs) as usize;
        // Son direct + réflexions.
        let mut h = vec![0.0; n];
        h[0] = self.direct_gain;
        for (d, g) in &self.reflections {
            let i = (d * fs).round() as usize;
            if i < n {
                h[i] += g * self.direct_gain;
            }
        }
        // Modes : filtrage par les résonances (biquads en cascade).
        for m in &self.modes {
            let b = Biquad::peaking(fs, m.freq, m.gain_db, m.q);
            h = filter_biquad(&b, &h);
        }
        // Queue diffuse : bruits à bande limitée, décroissance propre à chaque bande.
        if !self.tail.is_empty() {
            let level = self.direct_gain * fft::from_db(self.tail_level_db);
            let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
            let mut tail = vec![0.0; n];
            for band in &self.tail {
                let noise: Vec<f64> = (0..n)
                    .map(|i| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let u = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
                        let t = i as f64 / fs;
                        u * 10f64.powf(-3.0 * t / band.rt60_s)
                    })
                    .collect();
                let half = 2f64.sqrt();
                let bp = crate::analysis::bandpass_zero_phase(
                    &noise,
                    self.sample_rate,
                    band.center_hz / half,
                    band.center_hz * half,
                );
                for (t, v) in tail.iter_mut().zip(bp) {
                    *t += v;
                }
            }
            // Normalise la queue à `level` crête près de t = 0, puis l'ajoute
            // à partir de 1 ms après le son direct.
            let peak = tail[..(0.02 * fs) as usize]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()))
                .max(1e-12);
            let start = (0.001 * fs) as usize;
            for i in start..n {
                h[i] += tail[i - start] / peak * level;
            }
        }
        h
    }

    /// « Capture » simulée d'un signal joué : convolution par la pièce,
    /// retard d'arrivée, bruit de fond, silence de garde de `guard_s`.
    pub fn capture(&self, played: &[f64], guard_s: f64) -> Vec<f64> {
        let fs = self.sample_rate as f64;
        let h = self.impulse_response();
        let y = fft::convolve(played, &h);
        let delay = (self.direct_delay_s * fs).round() as usize;
        let total = delay + y.len() + (guard_s * fs) as usize;
        let mut out = vec![0.0; total];
        out[delay..delay + y.len()].copy_from_slice(&y);
        if self.noise_floor_dbfs.is_finite() {
            let amp = fft::from_db(self.noise_floor_dbfs) * 3f64.sqrt(); // uniforme → efficace
            let mut state = 0x1234_5678_9ABC_DEF0u64;
            for v in out.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let u = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
                *v += u * amp;
            }
        }
        out
    }

    /// Capture à travers un micro dont la réponse dévie (bandes de coloration),
    /// pour tester l'étalonnage : la correction doit annuler `mic_coloration`.
    pub fn capture_with_mic(
        &self,
        played: &[f64],
        guard_s: f64,
        mic_coloration: &[Band],
    ) -> Vec<f64> {
        let fs = self.sample_rate as f64;
        let mut y = self.capture(played, guard_s);
        for b in mic_coloration {
            let bq = match b.kind {
                BandType::Peak => Biquad::peaking(fs, b.freq, b.gain, b.q),
                BandType::LowShelf => Biquad::low_shelf(fs, b.freq, b.gain),
                BandType::HighShelf => Biquad::high_shelf(fs, b.freq, b.gain),
            };
            y = filter_biquad(&bq, &y);
        }
        y
    }

    /// Les modes exprimés comme bandes (sert à comparer avec l'égaliseur trouvé).
    pub fn modes_as_bands(&self) -> Vec<Band> {
        self.modes
            .iter()
            .map(|m| Band {
                freq: m.freq,
                gain: m.gain_db,
                q: m.q,
                kind: BandType::Peak,
                channel: None,
            })
            .collect()
    }
}

/// Filtrage IIR forme directe I (f64).
pub fn filter_biquad(b: &Biquad, x: &[f64]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    x.iter()
        .map(|&x0| {
            let y0 = b.b0 * x0 + b.b1 * x1 + b.b2 * x2 - b.a1 * y1 - b.a2 * y2;
            x2 = x1;
            x1 = x0;
            y2 = y1;
            y1 = y0;
            y0
        })
        .collect()
}

/// Applique une liste de bandes (l'égaliseur calculé) à un signal : sert à la
/// mesure de vérification « après correction ».
pub fn apply_bands(sample_rate: u32, bands: &[Band], x: &[f64]) -> Vec<f64> {
    let fs = sample_rate as f64;
    let mut y = x.to_vec();
    for b in bands {
        y = filter_biquad(&Biquad::from_band(fs, b), &y);
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piece_seche_a_la_bonne_longueur_et_un_pic_a_zero() {
        let r = RoomSim::dry_living_room(48_000);
        let h = r.impulse_response();
        assert_eq!(h.len(), 48_000);
        assert_eq!(fft::argmax_abs(&h), 0);
    }

    #[test]
    fn filtre_biquad_identite() {
        let b = Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        };
        let x = [1.0, -2.0, 3.0];
        assert_eq!(filter_biquad(&b, &x), x.to_vec());
    }
}
