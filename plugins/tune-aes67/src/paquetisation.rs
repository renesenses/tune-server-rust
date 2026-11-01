//! Le découpage du PCM en charges RTP de durée fixe.
//!
//! AES67 impose des paquets de durée constante : 1 ms, soit 48 trames à
//! 48 kHz. Le décodeur rend des blocs de taille quelconque ; le paquetiseur
//! garde le reste d'un bloc pour le suivant, de sorte qu'aucun échantillon ne
//! se perde ni ne se duplique à une frontière de bloc. En fin de flux, le
//! dernier paquet incomplet est complété par du silence : un paquet AES67 n'a
//! pas de longueur variable.

use crate::l24;

/// Trames par paquet pour une cadence et une durée de paquet données.
/// `None` si la durée ne tombe pas sur un nombre entier de trames.
pub fn trames_par_paquet(cadence: u32, duree_paquet_us: u32) -> Option<usize> {
    let produit = u64::from(cadence) * u64::from(duree_paquet_us);
    if produit == 0 || produit % 1_000_000 != 0 {
        return None;
    }
    usize::try_from(produit / 1_000_000).ok()
}

pub struct Paquetiseur {
    canaux: usize,
    trames_par_paquet: usize,
    en_attente: Vec<i32>,
}

impl Paquetiseur {
    pub fn new(canaux: usize, trames_par_paquet: usize) -> Self {
        assert!(canaux > 0 && trames_par_paquet > 0);
        Self {
            canaux,
            trames_par_paquet,
            en_attente: Vec::with_capacity(canaux * trames_par_paquet),
        }
    }

    /// Échantillons (entrelacés) par paquet.
    pub fn echantillons_par_paquet(&self) -> usize {
        self.canaux * self.trames_par_paquet
    }

    /// Octets de charge par paquet (L24).
    pub fn octets_par_paquet(&self) -> usize {
        self.echantillons_par_paquet() * l24::OCTETS_PAR_ECHANTILLON
    }

    /// Ajoute des échantillons 32 bits entrelacés et rend les charges L24
    /// complètes qu'ils permettent.
    pub fn pousser(&mut self, echantillons: &[i32]) -> Vec<Vec<u8>> {
        let n = self.echantillons_par_paquet();
        let mut charges = Vec::new();
        let mut reste = echantillons;
        if !self.en_attente.is_empty() {
            let manque = n - self.en_attente.len();
            let pris = manque.min(reste.len());
            self.en_attente.extend_from_slice(&reste[..pris]);
            reste = &reste[pris..];
            if self.en_attente.len() == n {
                let mut c = Vec::with_capacity(self.octets_par_paquet());
                l24::ecrire_l24(&self.en_attente, &mut c);
                charges.push(c);
                self.en_attente.clear();
            }
        }
        let mut morceaux = reste.chunks_exact(n);
        for m in &mut morceaux {
            let mut c = Vec::with_capacity(self.octets_par_paquet());
            l24::ecrire_l24(m, &mut c);
            charges.push(c);
        }
        self.en_attente.extend_from_slice(morceaux.remainder());
        charges
    }

    /// Échantillons en attente d'un paquet complet.
    pub fn en_attente(&self) -> usize {
        self.en_attente.len()
    }

    /// Fin de flux : le reste, complété de silence, en un dernier paquet.
    /// `None` s'il ne reste rien.
    pub fn vider(&mut self) -> Option<Vec<u8>> {
        if self.en_attente.is_empty() {
            return None;
        }
        self.en_attente.resize(self.echantillons_par_paquet(), 0);
        let mut c = Vec::with_capacity(self.octets_par_paquet());
        l24::ecrire_l24(&self.en_attente, &mut c);
        self.en_attente.clear();
        Some(c)
    }

    /// Oublie le reste (arrêt, saut) sans l'émettre.
    pub fn oublier(&mut self) {
        self.en_attente.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_milliseconde_a_48_khz_fait_48_trames() {
        assert_eq!(trames_par_paquet(48_000, 1_000), Some(48));
        assert_eq!(trames_par_paquet(96_000, 1_000), Some(96));
        assert_eq!(trames_par_paquet(48_000, 125), Some(6));
        assert_eq!(trames_par_paquet(44_100, 1_000), None, "44,1 trames");
        assert_eq!(trames_par_paquet(48_000, 0), None);
    }

    #[test]
    fn un_paquet_stereo_de_1_ms_porte_288_octets() {
        let p = Paquetiseur::new(2, 48);
        assert_eq!(p.octets_par_paquet(), 288);
    }

    #[test]
    fn les_frontieres_de_blocs_ne_perdent_ni_ne_dupliquent_rien() {
        // 1000 trames stéréo numérotées, poussées en blocs de tailles
        // irrégulières, doivent ressortir dans l'ordre, au complet.
        let source: Vec<i32> = (0..2000).map(|i| (i + 1) << 8).collect();
        let mut p = Paquetiseur::new(2, 48);
        let mut charges = Vec::new();
        let mut i = 0;
        for taille in [1, 95, 96, 97, 3, 500, 7].iter().cycle() {
            if i >= source.len() {
                break;
            }
            let fin = (i + taille).min(source.len());
            charges.extend(p.pousser(&source[i..fin]));
            i = fin;
        }
        assert_eq!(charges.len(), 2000 / 96);
        assert_eq!(p.en_attente(), 2000 % 96);
        charges.extend(p.vider());
        assert!(charges.iter().all(|c| c.len() == 288));
        let relu: Vec<i32> = charges.iter().flat_map(|c| l24::lire_l24(c)).collect();
        assert_eq!(&relu[..2000], &(1..=2000).collect::<Vec<i32>>()[..]);
        assert!(relu[2000..].iter().all(|&v| v == 0), "bourrage en silence");
        assert_eq!(p.vider(), None);
    }

    #[test]
    fn oublier_le_reste_ne_l_emet_pas() {
        let mut p = Paquetiseur::new(2, 48);
        assert!(p.pousser(&[1; 10]).is_empty());
        p.oublier();
        assert_eq!(p.vider(), None);
    }
}
