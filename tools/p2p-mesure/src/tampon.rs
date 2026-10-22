//! Tampon de lecture simulé (cahier § 3.1, question 3).
//!
//! Le client ne décode rien : il fait comme un lecteur qui consomme les octets
//! au débit du format. La lecture démarre quand le tampon contient `tampon_s`
//! secondes ; un **vidage** est compté chaque fois que le tampon tombe à zéro
//! pendant la lecture, et la lecture reprend quand il est de nouveau plein
//! (re-remplissage, comme un lecteur réel).
//!
//! La fin du flux n'est pas un vidage : le tampon se vide alors normalement.

#[derive(Debug, Clone)]
pub struct TamponSimule {
    /// Consommation, en octets par seconde.
    debit: f64,
    /// Seuil de (re)démarrage de la lecture, en octets.
    seuil: f64,
    niveau: f64,
    en_lecture: bool,
    /// Instant de la dernière mise à jour, en secondes.
    dernier: Option<f64>,
    sous_debits: u32,
}

impl TamponSimule {
    pub fn new(debit_bps: u64, tampon_s: f64) -> Self {
        let debit = debit_bps as f64 / 8.0;
        Self {
            debit,
            seuil: debit * tampon_s,
            niveau: 0.0,
            en_lecture: false,
            dernier: None,
            sous_debits: 0,
        }
    }

    /// Fait avancer la lecture jusqu'à `t` (secondes, horloge monotone).
    pub fn avancer(&mut self, t: f64) {
        let dt = match self.dernier {
            Some(d) if t > d => t - d,
            Some(_) => 0.0,
            None => 0.0,
        };
        self.dernier = Some(t.max(self.dernier.unwrap_or(t)));
        if self.en_lecture {
            self.niveau -= self.debit * dt;
            if self.niveau <= 0.0 {
                self.niveau = 0.0;
                self.en_lecture = false;
                self.sous_debits += 1;
            }
        }
    }

    /// `octets` reçus à l'instant `t`.
    pub fn recevoir(&mut self, t: f64, octets: usize) {
        self.avancer(t);
        self.niveau += octets as f64;
        if !self.en_lecture && self.niveau >= self.seuil {
            self.en_lecture = true;
        }
    }

    pub fn sous_debits(&self) -> u32 {
        self.sous_debits
    }

    #[cfg(test)]
    pub fn en_lecture(&self) -> bool {
        self.en_lecture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOC: usize = 64 * 1024;

    /// Rafale de 2 s puis blocs exactement au débit : jamais de vidage.
    #[test]
    fn flux_au_debit_ne_vide_jamais() {
        let debit_bps = 1_000_000u64;
        let mut t = TamponSimule::new(debit_bps, 2.0);
        let octets_s = debit_bps as f64 / 8.0;
        let rafale = (2.0 * octets_s / BLOC as f64).ceil() as usize;
        for _ in 0..rafale {
            t.recevoir(0.0, BLOC);
        }
        assert!(t.en_lecture());
        let mut instant = 0.0;
        for _ in 0..200 {
            instant += BLOC as f64 / octets_s;
            t.recevoir(instant, BLOC);
        }
        assert_eq!(t.sous_debits(), 0);
    }

    /// Une coupure de 3 s en cours de lecture vide un tampon de 2 s : un vidage,
    /// puis la lecture reprend quand le tampon est de nouveau plein.
    #[test]
    fn coupure_plus_longue_que_le_tampon_compte_un_vidage() {
        let debit_bps = 8_000_000u64; // 1 Mo/s
        let mut t = TamponSimule::new(debit_bps, 2.0);
        t.recevoir(0.0, 2_000_000);
        assert!(t.en_lecture());
        t.recevoir(3.0, 0);
        assert_eq!(t.sous_debits(), 1);
        assert!(!t.en_lecture());
        t.recevoir(3.5, 1_000_000);
        assert!(!t.en_lecture(), "pas de reprise avant 2 s de tampon");
        t.recevoir(4.0, 1_000_000);
        assert!(t.en_lecture());
        // Coupure de 1,5 s seulement : le tampon de 2 s tient.
        t.recevoir(5.5, 0);
        assert_eq!(t.sous_debits(), 1);
    }

    /// Le remplissage initial n'est pas un vidage.
    #[test]
    fn remplissage_initial_lent_n_est_pas_un_vidage() {
        let mut t = TamponSimule::new(8_000_000, 2.0);
        t.recevoir(0.0, 100);
        t.recevoir(10.0, 100);
        assert_eq!(t.sous_debits(), 0);
    }
}
