//! #5 — Suppression de la composante continue : passe-haut de Butterworth du
//! 2ᵉ ordre à 5 Hz, sur tous les canaux.

use super::filtres::{Coefs, FiltreAvecFondu, Q_BUTTERWORTH_2};
use super::{ErreurTraitement, GenreTraitement, Traitement, verifier_format};

pub const COUPURE_HZ: f64 = 5.0;

pub struct SuppressionContinu {
    coupure_hz: f64,
    socle: FiltreAvecFondu,
}

impl SuppressionContinu {
    pub fn new(coupure_hz: f64) -> Self {
        let coupure_hz = if coupure_hz.is_finite() {
            coupure_hz.clamp(1.0, 10.0)
        } else {
            COUPURE_HZ
        };
        Self {
            coupure_hz,
            socle: FiltreAvecFondu::default(),
        }
    }
}

impl Traitement for SuppressionContinu {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::SuppressionContinu
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        let s = vec![Coefs::passe_haut(
            self.coupure_hz,
            f64::from(cadence),
            Q_BUTTERWORTH_2,
        )];
        self.socle.preparer(s, usize::from(canaux), cadence);
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        self.socle.traiter(tampon, trames);
    }

    fn reinitialiser(&mut self) {
        self.socle.filtre.reinitialiser();
    }

    fn activer(&mut self, actif: bool) {
        self.socle.fondu.viser(actif);
    }

    fn sauter(&mut self) {
        self.socle.fondu.sauter();
    }

    fn actif(&self) -> bool {
        self.socle.fondu.actif()
    }

    fn est_neutre(&self) -> bool {
        self.socle.fondu.eteint_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    #[test]
    fn retire_le_continu_et_garde_le_reste() {
        let fs = 44_100.0;
        let trames = 4 * 44_100;
        let mut entree = sinus(1_000.0, fs, 0.4, trames, 2);
        entree.iter_mut().for_each(|x| *x += 0.05);
        let mut t = SuppressionContinu::new(COUPURE_HZ);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let sortie = par_blocs(&mut t, &entree, 2, 512);
        // Après 3 s, le continu a disparu…
        let fin = &sortie[3 * 44_100 * 2..];
        let moyenne = fin.iter().sum::<f64>() / fin.len() as f64;
        assert!(moyenne.abs() < 1e-4, "continu restant {moyenne}");
        // …et le 1 kHz est intact à 0,01 dB près.
        let a = amplitude_a(&sortie, 2, 0, 1_000.0, fs, 3 * 44_100, trames);
        assert!((20.0 * (a / 0.4).log10()).abs() < 0.01);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut SuppressionContinu::new(COUPURE_HZ), 2, 48_000);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut SuppressionContinu::new(COUPURE_HZ), 2, 44_100);
    }
}
