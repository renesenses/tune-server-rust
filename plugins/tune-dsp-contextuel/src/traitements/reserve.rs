//! #8 — Anti-écrêtage inter-échantillons : une réserve calée sur la CRÊTE
//! VRAIE mesurée (BS.1770, `tune_core::audio::crete_vraie`) au lieu des
//! −3 dB fixes. Gain = −max(0, crête vraie + marge) dB, posé par une rampe de
//! `RAMPE_MS`. Un master dont la crête vraie reste sous −marge n'est pas
//! touché du tout.

use super::{
    ErreurTraitement, GenreTraitement, RampeDeGain, Traitement, lineaire, verifier_format,
};

pub const MARGE_PAR_DEFAUT_DB: f64 = 0.3;
/// Au-delà, la mesure est suspecte : la réserve est bornée.
pub const RESERVE_MAX_DB: f64 = 6.0;

pub struct Reserve {
    reserve_db: f64,
    rampe: RampeDeGain,
    actif: bool,
    canaux: usize,
}

impl Reserve {
    pub fn new(crete_vraie_dbtp: f64, marge_db: f64) -> Self {
        let marge = if marge_db.is_finite() {
            marge_db.clamp(0.0, 3.0)
        } else {
            MARGE_PAR_DEFAUT_DB
        };
        let reserve_db = if crete_vraie_dbtp.is_finite() {
            (crete_vraie_dbtp + marge).clamp(0.0, RESERVE_MAX_DB)
        } else {
            0.0
        };
        Self {
            reserve_db,
            rampe: RampeDeGain::au_repos(1.0),
            actif: false,
            canaux: 0,
        }
    }

    /// La réserve appliquée une fois active, en dB (≥ 0).
    pub fn reserve_db(&self) -> f64 {
        self.reserve_db
    }
}

impl Traitement for Reserve {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Reserve
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        self.canaux = usize::from(canaux);
        self.rampe.preparer(cadence);
        self.rampe.sauter();
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        if self.rampe.unite_au_repos() || self.canaux == 0 {
            return;
        }
        for t in tampon[..trames * self.canaux].chunks_exact_mut(self.canaux) {
            let g = self.rampe.avancer();
            t.iter_mut().for_each(|x| *x *= g);
        }
    }

    fn reinitialiser(&mut self) {}

    fn activer(&mut self, actif: bool) {
        self.actif = actif;
        let g = if actif && self.reserve_db > 0.0 {
            lineaire(-self.reserve_db)
        } else {
            1.0
        };
        self.rampe.viser(g);
    }

    fn sauter(&mut self) {
        self.rampe.sauter();
    }

    fn actif(&self) -> bool {
        self.actif
    }

    fn est_neutre(&self) -> bool {
        self.rampe.unite_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;
    use tune_core::audio::crete_vraie::CreteVraie;

    #[test]
    fn reserve_calee_sur_la_crete_vraie() {
        assert!((Reserve::new(1.2, 0.3).reserve_db() - 1.5).abs() < 1e-12);
        assert_eq!(Reserve::new(-2.0, 0.3).reserve_db(), 0.0);
        assert_eq!(Reserve::new(12.0, 0.3).reserve_db(), RESERVE_MAX_DB);
        assert_eq!(Reserve::new(f64::NEG_INFINITY, 0.3).reserve_db(), 0.0);
    }

    #[test]
    fn plus_aucun_over_inter_echantillons_apres_la_reserve() {
        // fs/4 déphasé de 45° : échantillons à ±0,707·A, crête vraie A.
        // A = 1,3 : crête d'échantillon −0,75 dBFS, crête vraie +2,28 dBTP.
        let fs = 44_100usize;
        let a = 1.3;
        let mut entree = Vec::with_capacity(fs * 2);
        for n in 0..fs {
            let x =
                a * (std::f64::consts::FRAC_PI_2 * n as f64 + std::f64::consts::FRAC_PI_4).sin();
            entree.extend([x, x]);
        }
        let mut cv = CreteVraie::new(fs, 2);
        cv.nourrir(&entree);
        let crete_dbtp = 20.0 * cv.crete().log10();
        assert!(crete_dbtp > 2.0, "crête vraie {crete_dbtp}");

        let mut t = Reserve::new(crete_dbtp, MARGE_PAR_DEFAUT_DB);
        t.preparer(2, fs as u32).unwrap();
        t.activer(true);
        t.sauter();
        let sortie = par_blocs(&mut t, &entree, 2, 512);
        let mut apres = CreteVraie::new(fs, 2);
        apres.nourrir(&sortie);
        let db = 20.0 * apres.crete().log10();
        assert!(
            db <= -MARGE_PAR_DEFAUT_DB + 0.01,
            "crête vraie après : {db} dBTP"
        );
    }

    #[test]
    fn master_sous_le_seuil_n_est_pas_touche() {
        let mut t = Reserve::new(-1.0, MARGE_PAR_DEFAUT_DB);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        assert!(t.est_neutre());
        let entree = bruit(2_000, 2, 0.9, 9);
        assert_eq!(par_blocs(&mut t, &entree, 2, 64), entree);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Reserve::new(2.0, 0.3), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Reserve::new(3.0, 0.3), 2, 44_100);
    }
}
