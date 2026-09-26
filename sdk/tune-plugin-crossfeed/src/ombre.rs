//! #5081 — le filtre d'« ombre de la tête » du terme croisé.
//!
//! Il ne s'applique qu'au terme croisé `Rd − Ld` (voir `engine.rs`) : le Mid
//! reste intact, seul le Side est remodelé. Éteint par défaut ; éteint, le
//! moteur ne le traverse même pas.
//!
//! # La courbe visée
//!
//! Plate jusqu'à la fréquence de coupure `fc`, puis une pente de `s` dB par
//! octave, `s` de 3 à 6 :
//!
//! ```text
//! A(f) = 0                      pour f ≤ fc
//! A(f) = −s · log2(f / fc)      au-delà
//! ```
//!
//! # La réalisation
//!
//! Un filtre à phase minimale, en cascade de sections du 1er et du 2e ordre,
//! chacune transposée par la transformation bilinéaire avec sa fréquence
//! propre pré-déformée (elle tombe donc exactement là où on la place) :
//!
//! 1. un **genou** à `fc` : `(1 + s/ω) / (1 + s/(Qω) + s²/ω²)`, une pente de
//!    −6 dB/oct au-delà. À `Q = 0,5` c'est exactement le passe-bas du 1er
//!    ordre ; au-dessus, le coude se raidit (moins d'atténuation à `fc`) ;
//! 2. sous 6 dB/oct, une **cascade de plateaux** à la manière d'un filtre
//!    « rose » : un zéro à `fc·2^(k+α)`, un pôle à `fc·2^(k+1)`, pour
//!    `k = 0, 1, 2…`, avec `α = s/6`. Sur chaque octave la pente vaut −6 dB/oct
//!    pendant la fraction `α` et 0 le reste du temps : −`s` dB/oct en moyenne,
//!    avec une ondulation de quelques centièmes de dB.
//!
//! `Q` suit la pente : 0,5 à 3 dB/oct (le genou se réduit au pôle simple), 0,61
//! à 6 dB/oct. C'est la valeur qui tient la pente mesurée dans la tolérance
//! sans bosse sous `fc` (+0,09 dB au pire, sur le terme croisé seul).
//!
//! # Ce qui est mesuré, et l'écart
//!
//! La pente réelle, `(A(fc) − A(8·fc)) / 3`, est mesurée par les témoins de ce
//! module sur toute la grille 44,1 à 192 kHz, 200 Hz à 5 kHz, 3 à 6 dB/oct au
//! quart de dB/oct, tant que `8·fc ≤ fs/4` : l'écart à la consigne y reste
//! dans **[−0,42 ; +0,29] dB/oct**, pour une tolérance de ±0,5.
//!
//! Hors de ce domaine, deux écarts, assumés et dits :
//!
//! - **près de Nyquist**, la transformation bilinéaire comprime l'axe des
//!   fréquences : la pente se raidit au-delà de `fs/4` (de l'ordre de +2,6 dB/oct
//!   mesurés quand `8·fc` frôle `0,45·fs`). Le terme croisé y est déjà
//!   atténué de 20 dB ou plus ;
//! - une section dont la fréquence dépasse `0,45·fs` est omise : une coupure
//!   au-delà (19,8 kHz et plus à 44,1 kHz ; jamais dans la plage à 48 kHz
//!   et au-dessus) rend un filtre NEUTRE, le terme croisé passe tel quel.
//!
//! Le genou n'est pas un coude franc : à `fc`, le terme croisé est déjà
//! atténué de 1,3 dB (6 dB/oct) à 1,9 dB (3 dB/oct) — la moitié de l'erreur d'un coude
//! parfait, répartie de part et d'autre.

use std::f64::consts::PI;

/// Borne basse de la fréquence de coupure, en Hz.
pub const COUPURE_MIN_HZ: f32 = 200.0;
/// Borne haute de la fréquence de coupure, en Hz (Gold Note va jusque-là).
pub const COUPURE_MAX_HZ: f32 = 20_000.0;
/// Fréquence de coupure par défaut, en Hz.
pub const COUPURE_DEFAUT_HZ: f32 = 700.0;
/// Pente minimale, en dB par octave.
pub const PENTE_MIN_DB_OCT: f32 = 3.0;
/// Pente maximale, en dB par octave (passe-bas du 1er ordre).
pub const PENTE_MAX_DB_OCT: f32 = 6.0;
/// Pente par défaut, en dB par octave.
pub const PENTE_DEFAUT_DB_OCT: f32 = 6.0;

/// Au-delà de cette fraction du débit, une section n'est pas posée.
const PLAFOND_RELATIF: f64 = 0.45;
/// `Q` du genou à 3 dB/oct (pôle simple) et à 6 dB/oct.
const Q_A_3_DB: f64 = 0.5;
const Q_A_6_DB: f64 = 0.61;

/// Le réglage de l'ombre de la tête : où la pente commence, et combien.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OmbreDeTete {
    /// Fréquence de coupure, en Hz (200 à 20 000).
    pub cutoff_hz: f32,
    /// Pente au-delà de la coupure, en dB par octave (3 à 6).
    pub slope_db_per_octave: f32,
}

impl Default for OmbreDeTete {
    fn default() -> Self {
        Self {
            cutoff_hz: COUPURE_DEFAUT_HZ,
            slope_db_per_octave: PENTE_DEFAUT_DB_OCT,
        }
    }
}

impl OmbreDeTete {
    /// Le réglage ramené dans ses bornes ; un NaN retombe sur le défaut.
    pub fn bornee(self) -> Self {
        let cutoff = if self.cutoff_hz.is_finite() {
            self.cutoff_hz
        } else {
            COUPURE_DEFAUT_HZ
        };
        let pente = if self.slope_db_per_octave.is_finite() {
            self.slope_db_per_octave
        } else {
            PENTE_DEFAUT_DB_OCT
        };
        Self {
            cutoff_hz: cutoff.clamp(COUPURE_MIN_HZ, COUPURE_MAX_HZ),
            slope_db_per_octave: pente.clamp(PENTE_MIN_DB_OCT, PENTE_MAX_DB_OCT),
        }
    }
}

/// Une section du 2e ordre, forme directe II transposée, en `f64`.
#[derive(Debug, Clone)]
struct Section {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl Section {
    /// Transposition bilinéaire (`s = 2fs·(1 − z⁻¹)/(1 + z⁻¹)`) de
    /// `(n0 + n1·s + n2·s²) / (d0 + d1·s + d2·s²)`.
    fn bilineaire(n: [f64; 3], d: [f64; 3], fs: f64) -> Self {
        let k = 2.0 * fs;
        let k2 = k * k;
        let a0 = d[0] + d[1] * k + d[2] * k2;
        Self {
            b0: (n[0] + n[1] * k + n[2] * k2) / a0,
            b1: (2.0 * n[0] - 2.0 * n[2] * k2) / a0,
            b2: (n[0] - n[1] * k + n[2] * k2) / a0,
            a1: (2.0 * d[0] - 2.0 * d[2] * k2) / a0,
            a2: (d[0] - d[1] * k + d[2] * k2) / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    #[inline]
    fn traiter(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Réponse complexe à la pulsation normalisée `w` (rad/échantillon).
    fn reponse(&self, w: f64) -> (f64, f64) {
        let (c1, s1) = (w.cos(), -w.sin());
        let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin());
        let (nr, ni) = (
            self.b0 + self.b1 * c1 + self.b2 * c2,
            self.b1 * s1 + self.b2 * s2,
        );
        let (dr, di) = (
            1.0 + self.a1 * c1 + self.a2 * c2,
            self.a1 * s1 + self.a2 * s2,
        );
        let den = dr * dr + di * di;
        ((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
    }
}

/// La pulsation analogique qui tombe, après la bilinéaire, sur `f` Hz.
fn predeformee(f: f64, fs: f64) -> f64 {
    2.0 * fs * (PI * f / fs).tan()
}

/// Le filtre construit pour un débit donné, avec son état.
#[derive(Debug, Clone)]
pub(crate) struct FiltreOmbre {
    sections: Vec<Section>,
}

impl FiltreOmbre {
    /// Construire le filtre de `ombre` (déjà bornée) au débit `sample_rate`.
    pub(crate) fn concevoir(sample_rate: u32, ombre: OmbreDeTete) -> Self {
        let fs = f64::from(sample_rate.max(1));
        let plafond = PLAFOND_RELATIF * fs;
        let fc = f64::from(ombre.cutoff_hz);
        let alpha = f64::from(ombre.slope_db_per_octave) / 6.0;
        let mut sections = Vec::new();
        if fc < plafond {
            let q = Q_A_3_DB + (Q_A_6_DB - Q_A_3_DB) * (alpha - 0.5) / 0.5;
            let w = predeformee(fc, fs);
            sections.push(Section::bilineaire(
                [1.0, 1.0 / w, 0.0],
                [1.0, 1.0 / (q * w), 1.0 / (w * w)],
                fs,
            ));
            if alpha < 1.0 {
                let mut k = 0.0_f64;
                loop {
                    let zero = fc * 2.0_f64.powf(k + alpha);
                    let pole = fc * 2.0_f64.powf(k + 1.0);
                    if pole >= plafond {
                        break;
                    }
                    let (wz, wp) = (predeformee(zero, fs), predeformee(pole, fs));
                    sections.push(Section::bilineaire(
                        [1.0, 1.0 / wz, 0.0],
                        [1.0, 1.0 / wp, 0.0],
                        fs,
                    ));
                    k += 1.0;
                }
            }
        }
        Self { sections }
    }

    #[inline]
    pub(crate) fn traiter(&mut self, x: f64) -> f64 {
        self.sections.iter_mut().fold(x, |v, s| s.traiter(v))
    }

    /// Réponse complexe `(ré, im)` du filtre à `f` Hz.
    pub(crate) fn reponse(&self, f: f64, sample_rate: u32) -> (f64, f64) {
        let w = 2.0 * PI * f / f64::from(sample_rate.max(1));
        self.sections.iter().fold((1.0, 0.0), |(r, i), s| {
            let (sr, si) = s.reponse(w);
            (r * sr - i * si, r * si + i * sr)
        })
    }

    /// Reprendre l'état d'un filtre de MÊMES coefficients.
    pub(crate) fn reprendre_etat(&mut self, precedent: &Self) {
        for (s, p) in self.sections.iter_mut().zip(precedent.sections.iter()) {
            s.s1 = p.s1;
            s.s2 = p.s2;
        }
    }

    pub(crate) fn reinitialiser(&mut self) {
        for s in &mut self.sections {
            s.s1 = 0.0;
            s.s2 = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gain_db(f: &FiltreOmbre, hz: f64, sr: u32) -> f64 {
        let (r, i) = f.reponse(hz, sr);
        10.0 * (r * r + i * i).log10()
    }

    /// La pente MESURÉE entre `fc` et `8·fc`, en dB/oct.
    fn pente_mesuree(sr: u32, fc: f32, pente: f32) -> f64 {
        let f = FiltreOmbre::concevoir(
            sr,
            OmbreDeTete {
                cutoff_hz: fc,
                slope_db_per_octave: pente,
            },
        );
        let fc = f64::from(fc);
        (gain_db(&f, fc, sr) - gain_db(&f, 8.0 * fc, sr)) / 3.0
    }

    /// #5081 — la pente réelle, mesurée sur la réponse du filtre construit,
    /// reste à ±0,5 dB/oct de la consigne sur toute la grille où `8·fc` tient
    /// sous `fs/4`. Le témoin publie aussi les trois pentes de référence.
    #[test]
    fn la_pente_mesuree_suit_la_consigne_a_un_demi_db_pres_5081() {
        for (sr, fc) in [(48_000, 700.0_f32), (44_100, 1200.0)] {
            for consigne in [3.0_f32, 4.5, 6.0] {
                eprintln!(
                    "ombre {sr} Hz, fc {fc} Hz, consigne {consigne} dB/oct : mesurée {:.2} dB/oct",
                    pente_mesuree(sr, fc, consigne)
                );
            }
        }
        let mut pire = (0.0_f64, String::new());
        for sr in [44_100_u32, 48_000, 88_200, 96_000, 192_000] {
            for fc in [
                200.0_f32, 300.0, 500.0, 700.0, 1000.0, 1200.0, 1500.0, 2000.0, 3000.0, 5000.0,
            ] {
                if 8.0 * f64::from(fc) > f64::from(sr) / 4.0 {
                    continue;
                }
                for i in 0..=12 {
                    let consigne = 3.0 + 0.25 * i as f32;
                    let ecart = pente_mesuree(sr, fc, consigne) - f64::from(consigne);
                    if ecart.abs() > pire.0.abs() {
                        pire = (ecart, format!("{sr} Hz, fc {fc} Hz, consigne {consigne}"));
                    }
                    assert!(
                        ecart.abs() <= 0.5,
                        "pente mesurée hors tolérance : {ecart:+.3} dB/oct d'écart à la consigne \
                         ({sr} Hz, fc {fc} Hz, consigne {consigne} dB/oct)"
                    );
                }
            }
        }
        eprintln!("pire écart : {:+.3} dB/oct ({})", pire.0, pire.1);
    }

    /// Le filtre ne gonfle pas le terme croisé sous la coupure (au plus
    /// +0,1 dB), et rend le grave tel quel : 0 dB au continu.
    #[test]
    fn ni_bosse_sous_la_coupure_ni_perte_dans_le_grave_5081() {
        for sr in [44_100_u32, 96_000] {
            for consigne in [3.0_f32, 4.5, 6.0] {
                let f = FiltreOmbre::concevoir(
                    sr,
                    OmbreDeTete {
                        cutoff_hz: 700.0,
                        slope_db_per_octave: consigne,
                    },
                );
                assert!(gain_db(&f, 1.0, sr).abs() < 0.01, "continu");
                let mut hz = 20.0;
                while hz < 700.0 {
                    let g = gain_db(&f, hz, sr);
                    assert!(
                        g < 0.1,
                        "bosse de {g:+.3} dB à {hz:.0} Hz ({consigne} dB/oct)"
                    );
                    hz *= 1.05;
                }
            }
        }
    }

    /// Une coupure au-delà de 0,45·fs ne pose aucune section : neutre.
    #[test]
    fn une_coupure_au_dela_du_plafond_est_neutre_5081() {
        let f = FiltreOmbre::concevoir(
            44_100,
            OmbreDeTete {
                cutoff_hz: 20_000.0,
                slope_db_per_octave: 3.0,
            },
        );
        assert!(f.sections.is_empty());
    }

    #[test]
    fn les_bornes_et_le_nan_5081() {
        let b = OmbreDeTete {
            cutoff_hz: 50.0,
            slope_db_per_octave: 9.0,
        }
        .bornee();
        assert_eq!((b.cutoff_hz, b.slope_db_per_octave), (200.0, 6.0));
        let b = OmbreDeTete {
            cutoff_hz: f32::NAN,
            slope_db_per_octave: f32::NAN,
        }
        .bornee();
        assert_eq!(b, OmbreDeTete::default());
    }
}
