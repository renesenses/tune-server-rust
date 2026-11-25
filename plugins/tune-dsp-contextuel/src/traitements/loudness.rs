//! #7 — Loudness selon le volume (ISO 226:2003).
//!
//! À faible volume, l'oreille perd les graves et un peu de l'extrême aigu.
//! La compensation vaut la différence des courbes d'isosonie entre le niveau
//! d'écoute (`référence + volume_dB`) et la référence (80 phones à 0 dB de
//! volume par défaut, réglable), relative à 1 kHz.
//!
//! Elle est approchée par deux étagères (ajustement mesuré sur les courbes de
//! 40 à 70 phones) :
//! - basse à [`ETAGERE_BASSE_HZ`], gain = 1,09 × compensation à 40 Hz ;
//! - haute à [`ETAGERE_HAUTE_HZ`], gain = 0,93 × compensation à 12,5 kHz ;
//!
//! chacune bornée à `[0, plafond]`. Écart à la courbe : ≤ 2,5 dB jusqu'à
//! 20 dB sous la référence.
//!
//! Le volume se pose HORS TEMPS RÉEL ([`Traitement::definir_volume_db`]) et
//! change les coefficients entre deux blocs, états gardés : les gestes de
//! volume étant progressifs, l'écart entre deux réglages voisins est faible.
//! À volume ≥ 0 dB (écoute à la référence), les deux gains sont nuls et le
//! traitement est NEUTRE (fondu vers le sec, puis tampon intact).

use super::filtres::{Coefs, FiltreAvecFondu};
use super::{ErreurTraitement, GenreTraitement, Traitement, verifier_format};
use crate::iso226;

pub const REFERENCE_PHON: f64 = 80.0;
pub const PLAFOND_PAR_DEFAUT_DB: f64 = 3.0;
pub const PLAFOND_MAX_DB: f64 = 12.0;
pub const ETAGERE_BASSE_HZ: f64 = 150.0;
pub const ETAGERE_HAUTE_HZ: f64 = 10_000.0;
const PENTE_BASSE: f64 = 1.09;
const PENTE_HAUTE: f64 = 0.93;

/// Les gains des deux étagères (dB) pour un volume et une référence.
pub fn gains_etageres(volume_db: f64, reference_phon: f64, plafond_db: f64) -> (f64, f64) {
    if !volume_db.is_finite() || volume_db >= 0.0 {
        return (0.0, 0.0);
    }
    // Sous le seuil d'audition, les courbes n'ont plus de sens : 0 phone.
    let ecoute = (reference_phon + volume_db).max(0.0);
    let i40 = iso226::indice(40.0).expect("40 Hz dans le tableau");
    let i12k = iso226::indice(12_500.0).expect("12,5 kHz dans le tableau");
    let bas = PENTE_BASSE * iso226::compensation_db(i40, ecoute, reference_phon);
    let haut = PENTE_HAUTE * iso226::compensation_db(i12k, ecoute, reference_phon);
    (bas.clamp(0.0, plafond_db), haut.clamp(0.0, plafond_db))
}

pub struct Loudness {
    reference_phon: f64,
    plafond_db: f64,
    volume_db: f64,
    cadence: u32,
    socle: FiltreAvecFondu,
    /// Vrai quand l'utilisateur a activé le traitement (indépendamment du
    /// volume, qui peut le rendre neutre).
    demande: bool,
    gains: (f64, f64),
}

impl Loudness {
    pub fn new(reference_phon: f64, plafond_db: f64) -> Self {
        Self {
            reference_phon: if reference_phon.is_finite() {
                reference_phon.clamp(60.0, 100.0)
            } else {
                REFERENCE_PHON
            },
            plafond_db: if plafond_db.is_finite() {
                plafond_db.clamp(0.0, PLAFOND_MAX_DB)
            } else {
                PLAFOND_PAR_DEFAUT_DB
            },
            volume_db: 0.0,
            cadence: 0,
            socle: FiltreAvecFondu::default(),
            demande: false,
            gains: (0.0, 0.0),
        }
    }

    pub fn gains(&self) -> (f64, f64) {
        self.gains
    }

    pub fn reponse_db(&self, f: f64) -> f64 {
        self.socle.filtre.reponse_db(f, f64::from(self.cadence))
    }

    fn sections(&self) -> [Coefs; 2] {
        let fs = f64::from(self.cadence);
        [
            Coefs::etagere_basse(ETAGERE_BASSE_HZ, fs, self.gains.0),
            Coefs::etagere_haute(ETAGERE_HAUTE_HZ.min(0.4 * fs), fs, self.gains.1),
        ]
    }

    fn sans_effet(&self) -> bool {
        self.gains == (0.0, 0.0)
    }

    fn recaler(&mut self) {
        self.gains = gains_etageres(self.volume_db, self.reference_phon, self.plafond_db);
        if self.cadence > 0 {
            let s = self.sections();
            self.socle.filtre.changer_coefs(&s);
        }
        self.socle.fondu.viser(self.demande && !self.sans_effet());
    }
}

impl Traitement for Loudness {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Loudness
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        self.cadence = cadence;
        self.gains = gains_etageres(self.volume_db, self.reference_phon, self.plafond_db);
        let s = self.sections().to_vec();
        self.socle.preparer(s, usize::from(canaux), cadence);
        self.socle.fondu.viser(self.demande && !self.sans_effet());
        self.socle.fondu.sauter();
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        self.socle.traiter(tampon, trames);
    }

    fn reinitialiser(&mut self) {
        self.socle.filtre.reinitialiser();
    }

    fn activer(&mut self, actif: bool) {
        self.demande = actif;
        self.socle.fondu.viser(actif && !self.sans_effet());
    }

    fn sauter(&mut self) {
        self.socle.fondu.sauter();
    }

    fn actif(&self) -> bool {
        self.demande
    }

    fn est_neutre(&self) -> bool {
        self.socle.fondu.eteint_au_repos()
    }

    fn definir_volume_db(&mut self, volume_db: f64) {
        self.volume_db = volume_db;
        self.recaler();
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    #[test]
    fn neutre_au_volume_de_reference() {
        let mut t = Loudness::new(REFERENCE_PHON, PLAFOND_MAX_DB);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.definir_volume_db(0.0);
        t.sauter();
        assert!(t.est_neutre());
        let entree = bruit(4_000, 2, 0.9, 4);
        assert_eq!(par_blocs(&mut t, &entree, 2, 256), entree);
    }

    #[test]
    fn suit_la_courbe_iso_226_a_moins_20_db() {
        let mut t = Loudness::new(REFERENCE_PHON, PLAFOND_MAX_DB);
        t.preparer(2, 48_000).unwrap();
        t.activer(true);
        t.definir_volume_db(-20.0);
        for (i, &f) in iso226::FREQUENCES.iter().enumerate() {
            if f < 31.5 {
                continue;
            }
            let cible = iso226::compensation_db(i, 60.0, 80.0);
            let r = t.reponse_db(f);
            assert!((r - cible).abs() <= 2.5, "{f} Hz : {r} dB pour {cible} dB");
        }
        assert!(t.reponse_db(1_000.0).abs() < 0.5);
        assert!(t.reponse_db(40.0) > 6.0);
    }

    #[test]
    fn plus_le_volume_baisse_plus_les_graves_montent_jusqu_au_plafond() {
        let mut avant = 0.0;
        for v in [-5.0, -10.0, -20.0, -30.0] {
            let (bas, _) = gains_etageres(v, REFERENCE_PHON, PLAFOND_MAX_DB);
            assert!(bas > avant);
            avant = bas;
        }
        let (bas, haut) = gains_etageres(-60.0, REFERENCE_PHON, PLAFOND_PAR_DEFAUT_DB);
        assert_eq!(bas, PLAFOND_PAR_DEFAUT_DB);
        assert!(haut <= PLAFOND_PAR_DEFAUT_DB);
    }

    #[test]
    fn mesure_temporelle_des_graves() {
        let fs = 48_000.0;
        let mut t = Loudness::new(REFERENCE_PHON, PLAFOND_MAX_DB);
        t.preparer(2, 48_000).unwrap();
        t.activer(true);
        t.definir_volume_db(-20.0);
        t.sauter();
        let trames = 48_000;
        let sortie = par_blocs(&mut t, &sinus(40.0, fs, 0.1, trames, 2), 2, 480);
        let db = 20.0 * (amplitude_a(&sortie, 2, 0, 40.0, fs, trames / 2, trames) / 0.1).log10();
        assert!((db - t.reponse_db(40.0)).abs() < 0.05, "{db} dB");
    }

    #[test]
    fn identite_inactif() {
        let mut t = Loudness::new(REFERENCE_PHON, PLAFOND_PAR_DEFAUT_DB);
        t.definir_volume_db(-30.0);
        assert_identite_inactif(&mut t, 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        let mut t = Loudness::new(REFERENCE_PHON, PLAFOND_PAR_DEFAUT_DB);
        t.definir_volume_db(-30.0);
        assert_fondus_sans_saut(&mut t, 2, 44_100);
    }
}
