//! #4 — Correction de polarité : inverse un canal quand l'analyse a mesuré
//! les deux canaux en opposition de phase. Signalée par sa raison. La bascule
//! passe par un fondu : le canal passe par zéro en 20 ms au lieu de sauter.
//! Stéréo seulement ; neutre sinon.

use super::{Canal, ErreurTraitement, Fondu, GenreTraitement, Traitement, verifier_format};

pub struct Polarite {
    canal: Canal,
    fondu: Fondu,
    canaux: usize,
}

impl Polarite {
    pub fn new(canal: Canal) -> Self {
        Self {
            canal,
            fondu: Fondu::default(),
            canaux: 0,
        }
    }
}

impl Traitement for Polarite {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Polarite
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        self.canaux = usize::from(canaux);
        self.fondu.preparer(cadence);
        self.fondu.sauter();
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        if self.canaux != 2 {
            return;
        }
        let i = self.canal.indice();
        self.fondu.appliquer(tampon, trames, 2, |t| t[i] = -t[i]);
    }

    fn reinitialiser(&mut self) {}

    fn activer(&mut self, actif: bool) {
        self.fondu.viser(actif);
    }

    fn sauter(&mut self) {
        self.fondu.sauter();
    }

    fn actif(&self) -> bool {
        self.fondu.actif()
    }

    /// L'inversion ne change le niveau d'aucun canal.
    fn hausse_si(&self, _actif: bool) -> f64 {
        0.0
    }

    fn est_neutre(&self) -> bool {
        self.canaux != 2 || self.fondu.eteint_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    #[test]
    fn inverse_le_seul_canal_choisi() {
        for (canal, attendu) in [
            (Canal::Droit, vec![0.5, 0.25, -1.0, -0.0]),
            (Canal::Gauche, vec![-0.5, -0.25, 1.0, 0.0]),
        ] {
            let mut t = Polarite::new(canal);
            t.preparer(2, 44_100).unwrap();
            t.activer(true);
            t.sauter();
            let mut b = vec![0.5, -0.25, -1.0, 0.0];
            t.traiter(&mut b, 2);
            assert_eq!(b, attendu);
        }
    }

    #[test]
    fn deux_inversions_rendent_l_entree_au_bit_pres() {
        let entree = bruit(2_048, 2, 0.9, 5);
        let mut t = Polarite::new(Canal::Droit);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let une = par_blocs(&mut t, &entree, 2, 300);
        let deux = par_blocs(&mut t, &une, 2, 300);
        assert_eq!(deux, entree);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Polarite::new(Canal::Droit), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Polarite::new(Canal::Droit), 2, 44_100);
    }
}
