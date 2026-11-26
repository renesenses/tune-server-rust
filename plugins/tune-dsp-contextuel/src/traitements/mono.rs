//! #3 — Mono vrai : L = R = (L+R)/2. Moins de bruit sur un enregistrement
//! mono (ou double-mono) et une image parfaitement centrée. Sur un fichier
//! double-mono exact, la sortie est identique à l'entrée au bit près
//! (`(x + x) / 2 = x` en flottant). Stéréo seulement ; neutre sinon.

use super::{ErreurTraitement, Fondu, GenreTraitement, Traitement, verifier_format};

/// Hausse déclarée quand on coupe le mono (voir `hausse_si`).
pub const HAUSSE_EN_COUPANT_DB: f64 = 3.010_299_956_639_812;

#[derive(Default)]
pub struct Mono {
    fondu: Fondu,
    canaux: usize,
}

impl Mono {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Traitement for Mono {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Mono
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
        self.fondu.appliquer(tampon, trames, 2, |t| {
            let m = (t[0] + t[1]) * 0.5;
            t[0] = m;
            t[1] = m;
        });
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

    /// Allumer ne fait jamais monter l'énergie totale :
    /// `2·E((L+R)/2) = (E_L + E_R + 2C)/2 ≤ E_L + E_R` (Cauchy-Schwarz).
    /// Couper la rend : au plus ×2 (+3,01 dB) pour un programme à
    /// corrélation positive, le seul sur lequel la règle propose le mono.
    fn hausse_si(&self, actif: bool) -> f64 {
        if self.canaux == 2 && self.fondu.actif() && !actif {
            HAUSSE_EN_COUPANT_DB
        } else {
            0.0
        }
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
    fn somme_les_canaux() {
        let mut t = Mono::new();
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let mut b = vec![0.5, -0.25, 1.0, 0.0];
        t.traiter(&mut b, 2);
        assert_eq!(b, vec![0.125, 0.125, 0.5, 0.5]);
    }

    #[test]
    fn double_mono_exact_reste_au_bit_pres() {
        let mut t = Mono::new();
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let mut entree = bruit(4_096, 2, 0.9, 11);
        entree
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = t[0]);
        assert_eq!(par_blocs(&mut t, &entree, 2, 500), entree);
    }

    #[test]
    fn neutre_hors_stereo() {
        let mut t = Mono::new();
        t.preparer(6, 48_000).unwrap();
        t.activer(true);
        t.sauter();
        assert!(t.est_neutre());
        let entree = bruit(1_000, 6, 0.5, 2);
        assert_eq!(par_blocs(&mut t, &entree, 6, 100), entree);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Mono::new(), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Mono::new(), 2, 44_100);
    }
}
