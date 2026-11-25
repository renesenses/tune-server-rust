//! #2 — Filtre ultrasonique pour le DSD converti en PCM.
//!
//! La mise en forme du bruit du DSD repousse beaucoup d'énergie au-delà de
//! 20 kHz. Une fois le DSD converti en PCM par Tune (et SEULEMENT alors), un
//! passe-bas de Butterworth du 4ᵉ ordre, réglable de 20 à 50 kHz, l'écarte.
//!
//! ⛔ DSD natif et DoP ne sont jamais touchés : [`Ultrasonique::preparer`]
//! refuse toute origine autre que [`OrigineDsd::ConvertieEnPcm`]. Une coupure
//! au-delà de 0,45·fs n'a rien à filtrer : le traitement reste alors neutre.

use super::filtres::{FiltreAvecFondu, passe_bas_4};
use super::{ErreurTraitement, GenreTraitement, OrigineDsd, Traitement, verifier_format};

pub const COUPURE_MIN_HZ: f64 = 20_000.0;
pub const COUPURE_MAX_HZ: f64 = 50_000.0;
pub const COUPURE_PAR_DEFAUT_HZ: f64 = 30_000.0;

pub struct Ultrasonique {
    coupure_hz: f64,
    origine: OrigineDsd,
    socle: FiltreAvecFondu,
    /// Coupure au-delà de 0,45·fs : rien à faire.
    sans_objet: bool,
}

impl Ultrasonique {
    pub fn new(coupure_hz: f64, origine: OrigineDsd) -> Self {
        let coupure_hz = if coupure_hz.is_finite() {
            coupure_hz.clamp(COUPURE_MIN_HZ, COUPURE_MAX_HZ)
        } else {
            COUPURE_PAR_DEFAUT_HZ
        };
        Self {
            coupure_hz,
            origine,
            socle: FiltreAvecFondu::default(),
            sans_objet: true,
        }
    }

    pub fn coupure_hz(&self) -> f64 {
        self.coupure_hz
    }

    pub fn reponse_db(&self, f: f64, cadence: f64) -> f64 {
        self.socle.filtre.reponse_db(f, cadence)
    }
}

impl Traitement for Ultrasonique {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::UltrasoniqueDsd
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        if self.origine != OrigineDsd::ConvertieEnPcm {
            return Err(ErreurTraitement::OrigineNonPcm);
        }
        let fs = f64::from(cadence);
        self.sans_objet = self.coupure_hz >= 0.45 * fs;
        let sections = if self.sans_objet {
            Vec::new()
        } else {
            passe_bas_4(self.coupure_hz, fs)
        };
        self.socle.preparer(sections, usize::from(canaux), cadence);
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        if !self.sans_objet {
            self.socle.traiter(tampon, trames);
        }
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
        self.sans_objet || self.socle.fondu.eteint_au_repos()
    }
}

#[cfg(test)]
mod tests {
    use super::super::essais::*;
    use super::*;

    const DSD64_PCM: u32 = 176_400;

    #[test]
    fn refuse_le_dsd_natif_le_dop_et_le_pcm() {
        for origine in [OrigineDsd::Native, OrigineDsd::Dop, OrigineDsd::Aucune] {
            let mut t = Ultrasonique::new(30_000.0, origine);
            assert_eq!(
                t.preparer(2, DSD64_PCM),
                Err(ErreurTraitement::OrigineNonPcm)
            );
        }
    }

    #[test]
    fn coupe_l_ultrason_et_garde_l_audible() {
        let mut t = Ultrasonique::new(30_000.0, OrigineDsd::ConvertieEnPcm);
        t.preparer(2, DSD64_PCM).unwrap();
        let fs = f64::from(DSD64_PCM);
        assert!(t.reponse_db(10_000.0, fs).abs() < 0.01);
        assert!(t.reponse_db(20_000.0, fs).abs() < 0.2);
        assert!(t.reponse_db(60_000.0, fs) < -23.0);
        // Et en vrai, sur le signal.
        t.activer(true);
        t.sauter();
        let trames = 88_200;
        for (f, borne) in [(1_000.0, 0.01), (60_000.0, -23.0)] {
            t.reinitialiser();
            let sortie = par_blocs(&mut t, &sinus(f, fs, 0.5, trames, 2), 2, 1_024);
            let db = 20.0 * (amplitude_a(&sortie, 2, 0, f, fs, trames / 2, trames) / 0.5).log10();
            if f < 20_000.0 {
                assert!(db.abs() < borne, "{f} Hz : {db} dB");
            } else {
                assert!(db < borne, "{f} Hz : {db} dB");
            }
        }
    }

    #[test]
    fn coupure_au_dela_de_nyquist_reste_neutre() {
        let mut t = Ultrasonique::new(50_000.0, OrigineDsd::ConvertieEnPcm);
        t.preparer(2, 88_200).unwrap();
        t.activer(true);
        t.sauter();
        assert!(t.est_neutre());
        let entree = bruit(2_048, 2, 0.8, 3);
        let sortie = par_blocs(&mut t, &entree, 2, 256);
        assert_eq!(entree, sortie);
    }

    #[test]
    fn coupure_bornee_a_20_50_khz() {
        assert_eq!(
            Ultrasonique::new(5_000.0, OrigineDsd::ConvertieEnPcm).coupure_hz(),
            20_000.0
        );
        assert_eq!(
            Ultrasonique::new(90_000.0, OrigineDsd::ConvertieEnPcm).coupure_hz(),
            50_000.0
        );
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(
            &mut Ultrasonique::new(30_000.0, OrigineDsd::ConvertieEnPcm),
            2,
            DSD64_PCM,
        );
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(
            &mut Ultrasonique::new(30_000.0, OrigineDsd::ConvertieEnPcm),
            2,
            DSD64_PCM,
        );
    }
}
