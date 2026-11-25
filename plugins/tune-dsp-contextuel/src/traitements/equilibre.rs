//! #6 — Équilibre des canaux : compense l'écart de niveau L/R mesuré sur
//! l'album en ATTÉNUANT le canal le plus fort (jamais de hausse, donc jamais
//! d'écrêtage), au plus de [`PLAFOND_DB`]. Le gain glisse en `RAMPE_MS`.
//! À écart nul, ou coupé et revenu à l'unité, le tampon n'est pas touché.
//! Stéréo seulement ; neutre sinon.

use super::{
    ErreurTraitement, GenreTraitement, RampeDeGain, Traitement, lineaire, verifier_format,
};

pub const PLAFOND_DB: f64 = 2.0;

pub struct Equilibre {
    /// Canal atténué (0 = gauche, 1 = droit).
    canal: usize,
    /// Gain linéaire de compensation (≤ 1).
    gain: f64,
    rampe: RampeDeGain,
    actif: bool,
    canaux: usize,
}

impl Equilibre {
    /// `ecart_db` = niveau gauche − niveau droit (positif : gauche plus forte).
    pub fn new(ecart_db: f64) -> Self {
        let e = if ecart_db.is_finite() { ecart_db } else { 0.0 };
        let correction = e.abs().min(PLAFOND_DB);
        Self {
            canal: if e > 0.0 { 0 } else { 1 },
            gain: if correction == 0.0 {
                1.0
            } else {
                lineaire(-correction)
            },
            rampe: RampeDeGain::au_repos(1.0),
            actif: false,
            canaux: 0,
        }
    }

    /// La correction appliquée une fois active, en dB (≤ 0).
    pub fn correction_db(&self) -> f64 {
        20.0 * self.gain.log10()
    }

    pub fn canal_attenue(&self) -> usize {
        self.canal
    }
}

impl Traitement for Equilibre {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Equilibre
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        self.canaux = usize::from(canaux);
        self.rampe.preparer(cadence);
        self.rampe.sauter();
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        if self.canaux != 2 || self.rampe.unite_au_repos() {
            return;
        }
        let i = self.canal;
        for t in tampon[..trames * 2].as_chunks_mut::<2>().0 {
            t[i] *= self.rampe.avancer();
        }
    }

    fn reinitialiser(&mut self) {}

    fn activer(&mut self, actif: bool) {
        self.actif = actif;
        self.rampe.viser(if actif { self.gain } else { 1.0 });
    }

    fn sauter(&mut self) {
        self.rampe.sauter();
    }

    fn actif(&self) -> bool {
        self.actif
    }

    fn est_neutre(&self) -> bool {
        self.canaux != 2 || self.rampe.unite_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    #[test]
    fn attenue_le_canal_fort_et_plafonne() {
        let t = Equilibre::new(1.2);
        assert_eq!(t.canal_attenue(), 0);
        assert!((t.correction_db() + 1.2).abs() < 1e-9);
        let t = Equilibre::new(-5.0);
        assert_eq!(t.canal_attenue(), 1);
        assert!((t.correction_db() + PLAFOND_DB).abs() < 1e-9);
    }

    #[test]
    fn applique_le_gain_apres_la_rampe_sans_toucher_l_autre_canal() {
        let mut t = Equilibre::new(1.0);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        let entree = sinus(440.0, 44_100.0, 0.5, 44_100, 2);
        let sortie = par_blocs(&mut t, &entree, 2, 256);
        // Le canal droit n'est jamais touché.
        assert!(
            entree
                .iter()
                .skip(1)
                .step_by(2)
                .zip(sortie.iter().skip(1).step_by(2))
                .all(|(a, b)| a == b)
        );
        // Après la rampe (20 ms), le gauche est à −1 dB.
        let n = 10_000;
        assert!((sortie[2 * n] - entree[2 * n] * lineaire(-1.0)).abs() < 1e-12);
    }

    #[test]
    fn ecart_nul_reste_neutre_meme_actif() {
        let mut t = Equilibre::new(0.0);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        assert!(t.est_neutre());
        let entree = bruit(2_000, 2, 0.9, 1);
        assert_eq!(par_blocs(&mut t, &entree, 2, 128), entree);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Equilibre::new(1.5), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Equilibre::new(2.0), 2, 44_100);
    }
}
