//! #9 — Rumble vinyle : passe-haut de Butterworth du 4ᵉ ordre réglable de 15
//! à 20 Hz (grondement de la platine, voile du disque), et en option le
//! filtre de « voile » elliptique : la composante latérale (L−R)/2 passe dans
//! un passe-haut du 2ᵉ ordre à [`VOILE_HZ`] — les graves sont recentrés,
//! ce qui retire le grondement vertical du sillon sans toucher au centre.
//! L'option elliptique ne vaut que pour la stéréo.

use super::filtres::{Cascade, Coefs, FiltreAvecFondu, Q_BUTTERWORTH_2, passe_haut_4};
use super::{ErreurTraitement, GenreTraitement, Traitement, verifier_format};

pub const COUPURE_MIN_HZ: f64 = 15.0;
pub const COUPURE_MAX_HZ: f64 = 20.0;
pub const COUPURE_PAR_DEFAUT_HZ: f64 = 18.0;
pub const VOILE_HZ: f64 = 120.0;

pub struct Rumble {
    coupure_hz: f64,
    voile: bool,
    socle: FiltreAvecFondu,
    /// Passe-haut de la composante latérale (un seul « canal »).
    lateral: Cascade,
}

impl Rumble {
    pub fn new(coupure_hz: f64, voile: bool) -> Self {
        let coupure_hz = if coupure_hz.is_finite() {
            coupure_hz.clamp(COUPURE_MIN_HZ, COUPURE_MAX_HZ)
        } else {
            COUPURE_PAR_DEFAUT_HZ
        };
        Self {
            coupure_hz,
            voile,
            socle: FiltreAvecFondu::default(),
            lateral: Cascade::default(),
        }
    }

    pub fn coupure_hz(&self) -> f64 {
        self.coupure_hz
    }
}

impl Traitement for Rumble {
    fn genre(&self) -> GenreTraitement {
        GenreTraitement::Rumble
    }

    fn preparer(&mut self, canaux: u16, cadence: u32) -> Result<(), ErreurTraitement> {
        verifier_format(canaux, cadence)?;
        let fs = f64::from(cadence);
        self.socle.preparer(
            passe_haut_4(self.coupure_hz, fs),
            usize::from(canaux),
            cadence,
        );
        let voile = self.voile && canaux == 2;
        self.lateral = Cascade::new(
            if voile {
                vec![Coefs::passe_haut(VOILE_HZ, fs, Q_BUTTERWORTH_2)]
            } else {
                Vec::new()
            },
            1,
        );
        Ok(())
    }

    fn traiter(&mut self, tampon: &mut [f64], trames: usize) {
        let FiltreAvecFondu {
            filtre,
            fondu,
            canaux,
        } = &mut self.socle;
        let lateral = &mut self.lateral;
        let voile = !lateral.sections().is_empty();
        let tourne = fondu.appliquer(tampon, trames, *canaux, |trame| {
            for (c, x) in trame.iter_mut().enumerate() {
                *x = filtre.traiter(c, *x);
            }
            if voile {
                let m = (trame[0] + trame[1]) * 0.5;
                let s = lateral.traiter(0, (trame[0] - trame[1]) * 0.5);
                trame[0] = m + s;
                trame[1] = m - s;
            }
        });
        if !tourne {
            filtre.reinitialiser();
            lateral.reinitialiser();
        }
    }

    fn reinitialiser(&mut self) {
        self.socle.filtre.reinitialiser();
        self.lateral.reinitialiser();
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

    fn niveau_db(t: &mut Rumble, entree: &[f64], f: f64, canal: usize, ref_amp: f64) -> f64 {
        let trames = entree.len() / 2;
        let sortie = par_blocs(t, entree, 2, 512);
        20.0 * (amplitude_a(&sortie, 2, canal, f, 44_100.0, trames / 2, trames) / ref_amp).log10()
    }

    #[test]
    fn coupe_le_grondement_garde_la_musique() {
        let fs = 44_100.0;
        let trames = 4 * 44_100;
        let mut t = Rumble::new(18.0, false);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let grondement = niveau_db(&mut t, &sinus(5.0, fs, 0.5, trames, 2), 5.0, 0, 0.5);
        assert!(grondement < -40.0, "5 Hz : {grondement} dB");
        t.reinitialiser();
        let musique = niveau_db(&mut t, &sinus(1_000.0, fs, 0.5, trames, 2), 1_000.0, 0, 0.5);
        assert!(musique.abs() < 0.01, "1 kHz : {musique} dB");
    }

    #[test]
    fn voile_recentre_les_graves_lateraux() {
        let fs = 44_100.0;
        let trames = 2 * 44_100;
        // Composante latérale pure à 50 Hz : L = s, R = −s.
        let mut lateral = sinus(50.0, fs, 0.5, trames, 2);
        lateral
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .for_each(|t| t[1] = -t[1]);
        let mut t = Rumble::new(18.0, true);
        t.preparer(2, 44_100).unwrap();
        t.activer(true);
        t.sauter();
        let s = niveau_db(&mut t, &lateral, 50.0, 0, 0.5);
        assert!(s < -10.0, "latéral 50 Hz : {s} dB");
        // Le centre (mono) à 50 Hz passe, au rumble près (−0,0x dB).
        t.reinitialiser();
        let m = niveau_db(&mut t, &sinus(50.0, fs, 0.5, trames, 2), 50.0, 0, 0.5);
        assert!(m.abs() < 0.1, "centre 50 Hz : {m} dB");
    }

    #[test]
    fn coupure_bornee() {
        assert_eq!(Rumble::new(5.0, false).coupure_hz(), 15.0);
        assert_eq!(Rumble::new(40.0, false).coupure_hz(), 20.0);
    }

    #[test]
    fn identite_inactif() {
        assert_identite_inactif(&mut Rumble::new(18.0, true), 2, 44_100);
    }

    #[test]
    fn fondu_sans_saut() {
        assert_fondus_sans_saut(&mut Rumble::new(18.0, true), 2, 44_100);
    }
}
