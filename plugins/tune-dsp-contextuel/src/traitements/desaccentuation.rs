//! #1 — Désaccentuation CD 50/15 µs (IEC 60908).
//!
//! Certains CD (surtout 1983–1990) ont été gravés avec une pré-accentuation
//! des aigus, signalée par le bit 0x01 du champ CONTROL de la TOC (ou
//! `FLAGS PRE` dans une feuille CUE). Le lecteur doit appliquer la courbe
//! inverse ; Tune ne le faisait pas, ces disques sonnaient trop brillants.
//!
//! Courbe analogique : `H(s) = (1 + s·τ₂) / (1 + s·τ₁)`, τ₁ = 50 µs,
//! τ₂ = 15 µs ; 0 dB au continu, −10,46 dB à l'infini.
//!
//! Filtre numérique : un pôle et un zéro réels, gain unité au continu. La
//! transformation bilinéaire, même prédéformée, s'écarte de 2,7 dB à 20 kHz
//! sous 44,1 kHz ; on cherche donc le pôle et le zéro qui minimisent l'écart
//! MAXIMAL (en dB) à la courbe analogique sur 20 Hz – min(20 kHz, 0,45·fs),
//! par descente de coordonnées depuis l'appariement des pôles (`e^{-1/(τ·fs)}`).
//! Calcul déterministe, hors temps réel, quelques millisecondes. Écart obtenu :
//! 0,094 dB à 44,1 kHz, 0,065 dB à 48 kHz, < 0,005 dB dès 88,2 kHz.

use super::filtres::{Cascade, Coefs};
use super::{ErreurTraitement, Fondu, GenreTraitement, Traitement, verifier_format};

pub const TAU_1: f64 = 50e-6;
pub const TAU_2: f64 = 15e-6;

/// Gain de la courbe analogique à `f` Hz, en dB.
pub fn gain_theorique_db(f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f;
    let num = 1.0 + (w * TAU_2).powi(2);
    let den = 1.0 + (w * TAU_1).powi(2);
    10.0 * (num / den).log10()
}

fn coefs_pole_zero(p: f64, q: f64) -> Coefs {
    let g = (1.0 - p) / (1.0 - q);
    Coefs {
        b0: g,
        b1: -g * q,
        b2: 0.0,
        a1: -p,
        a2: 0.0,
    }
}

/// Écart maximal, en dB, entre le filtre et la courbe analogique, sur une
/// grille logarithmique (pas de 2 %) de 20 Hz à min(20 kHz, 0,45·fs).
pub fn ecart_max_db(c: &Coefs, cadence: f64) -> f64 {
    let haut = 20_000f64.min(0.45 * cadence);
    let mut f = 20.0;
    let mut m = 0.0f64;
    while f <= haut {
        m = m.max((c.reponse_db(f, cadence) - gain_theorique_db(f)).abs());
        f *= 1.02;
    }
    m
}

/// Les coefficients du filtre pour une cadence (voir le module).
pub fn coefficients(cadence: u32) -> Coefs {
    let fs = f64::from(cadence);
    let mut p = (-1.0 / (TAU_1 * fs)).exp();
    let mut q = (-1.0 / (TAU_2 * fs)).exp();
    let mut meilleur = ecart_max_db(&coefs_pole_zero(p, q), fs);
    let mut pas = 0.05;
    while pas > 1e-7 {
        let mut ameliore = false;
        for (dp, dq) in [(pas, 0.0), (-pas, 0.0), (0.0, pas), (0.0, -pas)] {
            let (pp, qq) = (p + dp, q + dq);
            if pp.abs() < 0.999 && qq.abs() < 0.999 {
                let e = ecart_max_db(&coefs_pole_zero(pp, qq), fs);
                if e < meilleur {
                    meilleur = e;
                    p = pp;
                    q = qq;
                    ameliore = true;
                }
            }
        }
        if !ameliore {
            pas /= 2.0;
        }
    }
    coefs_pole_zero(p, q)
}

pub struct Desaccentuation {
    filtre: Cascade,
    fondu: Fondu,
    canaux: usize,
}

impl Desaccentuation {
    pub fn new() -> Self {
        Self {
            filtre: Cascade::default(),
            fondu: Fondu::default(),
            canaux: 0,
        }
    }

    pub fn filtre(&self) -> &Cascade {
        &self.filtre
    }
}

impl Default for Desaccentuation {
    fn default() -> Self {
        Self::new()
    }
}

impl Traitement for Desaccentuation {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Desaccentuation
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        self.canaux = usize::from(canaux);
        self.filtre = Cascade::new(vec![coefficients(cadence)], self.canaux);
        self.fondu.preparer(cadence);
        self.fondu.sauter();
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
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

    fn reinitialiser(&mut self) {
        self.filtre.reinitialiser();
    }

    fn activer(&mut self, actif: bool) {
        self.fondu.viser(actif);
    }

    fn sauter(&mut self) {
        self.fondu.sauter();
    }

    fn actif(&self) -> bool {
        self.fondu.actif()
    }

    fn est_neutre(&self) -> bool {
        self.fondu.eteint_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    #[test]
    fn courbe_theorique_connue() {
        assert!(gain_theorique_db(0.0).abs() < 1e-12);
        // Valeurs publiées de la courbe 50/15 µs : −4,53 dB à 5 kHz,
        // −9,04 dB à 16 kHz ; asymptote 20·log10(15/50) = −10,46 dB.
        assert!((gain_theorique_db(5_000.0) + 4.53).abs() < 0.01);
        assert!((gain_theorique_db(16_000.0) + 9.04).abs() < 0.01);
        assert!((gain_theorique_db(1e9) + 10.457).abs() < 0.001);
    }

    #[test]
    fn reponse_suit_la_courbe_a_44_1_et_48_khz() {
        for cadence in [44_100u32, 48_000] {
            let c = coefficients(cadence);
            let e = ecart_max_db(&c, f64::from(cadence));
            assert!(e <= 0.1, "{cadence} Hz : écart {e} dB");
            // Gain unité au continu.
            assert!(c.reponse_db(0.0, f64::from(cadence)).abs() < 1e-9);
        }
    }

    #[test]
    fn mesure_temporelle_contre_la_courbe() {
        // Le filtre réel, sur des sinusoïdes, et non sa seule formule.
        for cadence in [44_100u32, 48_000] {
            let fs = f64::from(cadence);
            for f in [100.0, 1_000.0, 5_000.0, 10_000.0, 16_000.0] {
                let mut t = Desaccentuation::new();
                t.preparer(2, cadence).unwrap();
                t.activer(true);
                t.sauter();
                let trames = cadence as usize; // 1 s
                let entree = sinus(f, fs, 0.5, trames, 2);
                let sortie = par_blocs(&mut t, &entree, 2, 512);
                let a = amplitude_a(&sortie, 2, 1, f, fs, trames / 2, trames);
                let mesure = 20.0 * (a / 0.5).log10();
                let ecart = mesure - gain_theorique_db(f);
                assert!(
                    ecart.abs() <= 0.1,
                    "{cadence} Hz, {f} Hz : écart {ecart} dB"
                );
            }
        }
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Desaccentuation::new(), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Desaccentuation::new(), 2, 44_100);
    }
}
