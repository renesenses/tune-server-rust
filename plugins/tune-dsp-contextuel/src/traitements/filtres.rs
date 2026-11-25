//! Biquads (forme transposée II, `f64`) et leurs calculs : passe-haut et
//! passe-bas de Butterworth, étagères (Audio EQ Cookbook de R. Bristow-Johnson),
//! et la réponse en fréquence qui sert aux tests.

use std::f64::consts::PI;

/// Facteurs de qualité d'un Butterworth du 4ᵉ ordre en deux biquads.
pub const Q_BUTTERWORTH_4: [f64; 2] = [0.541_196_100_146_197, 1.306_562_964_876_376_4];
/// Facteur de qualité d'un Butterworth du 2ᵉ ordre.
pub const Q_BUTTERWORTH_2: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Coefficients normalisés (`a0` = 1) d'une section du 2ᵉ ordre ; une section
/// du 1ᵉʳ ordre a `b2` = `a2` = 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coefs {
    pub const IDENTITE: Coefs = Coefs {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    fn normaliser(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b0: b[0] / a[0],
            b1: b[1] / a[0],
            b2: b[2] / a[0],
            a1: a[1] / a[0],
            a2: a[2] / a[0],
        }
    }

    pub fn passe_haut(fc: f64, cadence: f64, q: f64) -> Self {
        let (c, alpha) = cos_alpha(fc, cadence, q);
        Self::normaliser(
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        )
    }

    pub fn passe_bas(fc: f64, cadence: f64, q: f64) -> Self {
        let (c, alpha) = cos_alpha(fc, cadence, q);
        Self::normaliser(
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        )
    }

    /// Étagère basse, pente S = 1.
    pub fn etagere_basse(fc: f64, cadence: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (c, alpha) = cos_alpha(fc, cadence, Q_BUTTERWORTH_2);
        let s = 2.0 * a.sqrt() * alpha;
        Self::normaliser(
            [
                a * ((a + 1.0) - (a - 1.0) * c + s),
                2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                a * ((a + 1.0) - (a - 1.0) * c - s),
            ],
            [
                (a + 1.0) + (a - 1.0) * c + s,
                -2.0 * ((a - 1.0) + (a + 1.0) * c),
                (a + 1.0) + (a - 1.0) * c - s,
            ],
        )
    }

    /// Étagère haute, pente S = 1.
    pub fn etagere_haute(fc: f64, cadence: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (c, alpha) = cos_alpha(fc, cadence, Q_BUTTERWORTH_2);
        let s = 2.0 * a.sqrt() * alpha;
        Self::normaliser(
            [
                a * ((a + 1.0) + (a - 1.0) * c + s),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - s),
            ],
            [
                (a + 1.0) - (a - 1.0) * c + s,
                2.0 * ((a - 1.0) - (a + 1.0) * c),
                (a + 1.0) - (a - 1.0) * c - s,
            ],
        )
    }

    /// Module de la réponse à `f` Hz, en dB.
    pub fn reponse_db(&self, f: f64, cadence: f64) -> f64 {
        let w = 2.0 * PI * f / cadence;
        // z⁻¹ = e^{-jw}, z⁻² = e^{-2jw}
        let (c1, s1) = (w.cos(), -w.sin());
        let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin());
        let num = (
            self.b0 + self.b1 * c1 + self.b2 * c2,
            self.b1 * s1 + self.b2 * s2,
        );
        let den = (
            1.0 + self.a1 * c1 + self.a2 * c2,
            self.a1 * s1 + self.a2 * s2,
        );
        10.0 * ((num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)).log10()
    }
}

fn cos_alpha(fc: f64, cadence: f64, q: f64) -> (f64, f64) {
    let w0 = 2.0 * PI * fc / cadence;
    (w0.cos(), w0.sin() / (2.0 * q))
}

/// Une cascade de sections, avec un état par canal et par section.
#[derive(Debug, Clone, Default)]
pub struct Cascade {
    sections: Vec<Coefs>,
    /// `etats[canal * sections + s]` = (s1, s2) de la forme transposée II.
    etats: Vec<[f64; 2]>,
}

impl Cascade {
    /// HORS TEMPS RÉEL.
    pub fn new(sections: Vec<Coefs>, canaux: usize) -> Self {
        let etats = vec![[0.0; 2]; sections.len() * canaux];
        Self { sections, etats }
    }

    pub fn sections(&self) -> &[Coefs] {
        &self.sections
    }

    /// HORS TEMPS RÉEL ou entre deux blocs : remplace les coefficients en
    /// gardant les états (même nombre de sections).
    pub fn changer_coefs(&mut self, sections: &[Coefs]) {
        debug_assert_eq!(sections.len(), self.sections.len());
        self.sections.copy_from_slice(sections);
    }

    pub fn reinitialiser(&mut self) {
        self.etats.iter_mut().for_each(|e| *e = [0.0; 2]);
    }

    /// TEMPS RÉEL : un échantillon du canal `canal`.
    #[inline]
    pub fn traiter(&mut self, canal: usize, mut x: f64) -> f64 {
        let n = self.sections.len();
        let etats = &mut self.etats[canal * n..(canal + 1) * n];
        for (c, e) in self.sections.iter().zip(etats.iter_mut()) {
            let y = c.b0 * x + e[0];
            e[0] = c.b1 * x - c.a1 * y + e[1];
            e[1] = c.b2 * x - c.a2 * y;
            x = y;
        }
        x
    }

    /// Réponse de la cascade entière, en dB.
    pub fn reponse_db(&self, f: f64, cadence: f64) -> f64 {
        self.sections.iter().map(|c| c.reponse_db(f, cadence)).sum()
    }
}

/// Deux biquads de Butterworth en cascade (4ᵉ ordre), passe-haut.
pub fn passe_haut_4(fc: f64, cadence: f64) -> Vec<Coefs> {
    Q_BUTTERWORTH_4
        .iter()
        .map(|&q| Coefs::passe_haut(fc, cadence, q))
        .collect()
}

/// Deux biquads de Butterworth en cascade (4ᵉ ordre), passe-bas.
pub fn passe_bas_4(fc: f64, cadence: f64) -> Vec<Coefs> {
    Q_BUTTERWORTH_4
        .iter()
        .map(|&q| Coefs::passe_bas(fc, cadence, q))
        .collect()
}

/// Une cascade appliquée à tous les canaux, derrière un [`Fondu`] : le socle
/// des traitements « filtre » (désaccentuation exceptée, qui garde le sien
/// pour exposer son filtre). Éteint au repos : rien n'est touché, les états
/// sont vidés pour repartir propres à la prochaine activation.
#[derive(Debug, Clone, Default)]
pub struct FiltreAvecFondu {
    pub filtre: Cascade,
    pub fondu: super::Fondu,
    pub canaux: usize,
}

impl FiltreAvecFondu {
    /// HORS TEMPS RÉEL.
    pub fn preparer(&mut self, sections: Vec<Coefs>, canaux: usize, cadence: u32) {
        self.canaux = canaux;
        self.filtre = Cascade::new(sections, canaux);
        self.fondu.preparer(cadence);
        self.fondu.sauter();
    }

    pub fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        let filtre = &mut self.filtre;
        let tourne = self.fondu.appliquer(tampon, trames, self.canaux, |trame| {
            for (c, x) in trame.iter_mut().enumerate() {
                *x = filtre.traiter(c, *x);
            }
        });
        if !tourne {
            filtre.reinitialiser();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn butterworth_4_vaut_moins_3_db_a_la_coupure() {
        let c = Cascade::new(passe_haut_4(18.0, 44_100.0), 1);
        assert!((c.reponse_db(18.0, 44_100.0) + 3.0103).abs() < 0.01);
        let c = Cascade::new(passe_bas_4(30_000.0, 176_400.0), 1);
        assert!((c.reponse_db(30_000.0, 176_400.0) + 3.0103).abs() < 0.01);
    }

    #[test]
    fn etageres_a_gain_nul_sont_plates() {
        for c in [
            Coefs::etagere_basse(150.0, 48_000.0, 0.0),
            Coefs::etagere_haute(10_000.0, 48_000.0, 0.0),
        ] {
            for f in [20.0, 1_000.0, 20_000.0] {
                assert!(c.reponse_db(f, 48_000.0).abs() < 1e-9);
            }
        }
    }
}
