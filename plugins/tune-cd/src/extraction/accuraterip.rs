//! Les CRC AccurateRip v1 et v2 d'une piste (#2466), calculés au fil de la
//! lecture.
//!
//! Chaque trame stéréo 16 bits est lue comme un mot de 32 bits petit-boutiste
//! (gauche dans les 16 bits bas), multiplié par sa position (1, 2, 3…) :
//!
//! * v1 : somme des produits, modulo 2³² ;
//! * v2 : somme des moitiés haute ET basse de chaque produit sur 64 bits,
//!   modulo 2³².
//!
//! Sur la première piste, les positions avant `5 × 588` sont sautées ; sur
//! la dernière, les `5 × 588` dernières (la règle de l'algorithme publié :
//! un lecteur décalé ne lit pas ces trames-là).
//!
//! Hors de cette version : la comparaison avec la base AccurateRip, qui
//! demande l'identifiant AccurateRip du disque ET le décalage de lecture du
//! lecteur. Les CRC rangés ici sont ceux des secteurs lus, SANS correction
//! du décalage.

/// `5 × 2 352 / 4` : trames sautées en tête de la première piste et en fin
/// de la dernière.
pub const TRAMES_SAUTEES: u32 = 2_940;

#[derive(Debug, Clone)]
pub struct CalculAccurateRip {
    /// Position de la prochaine trame (la première vaut 1).
    position: u32,
    depuis: u32,
    jusqu_a: u32,
    v1: u32,
    v2: u32,
}

impl CalculAccurateRip {
    /// `trames` : nombre total de trames stéréo de la piste.
    pub fn new(trames: u32, premiere: bool, derniere: bool) -> Self {
        Self {
            position: 1,
            depuis: if premiere { TRAMES_SAUTEES } else { 0 },
            jusqu_a: if derniere {
                trames.saturating_sub(TRAMES_SAUTEES)
            } else {
                trames
            },
            v1: 0,
            v2: 0,
        }
    }

    /// Ajoute des octets PCM (multiple de 4 : les blocs sont des secteurs).
    pub fn ajouter(&mut self, pcm: &[u8]) {
        for t in pcm.as_chunks::<4>().0 {
            let mot = u32::from_le_bytes(*t);
            if self.position >= self.depuis && self.position <= self.jusqu_a {
                self.v1 = self.v1.wrapping_add(mot.wrapping_mul(self.position));
                let p = mot as u64 * self.position as u64;
                self.v2 = self
                    .v2
                    .wrapping_add(p as u32)
                    .wrapping_add((p >> 32) as u32);
            }
            self.position = self.position.wrapping_add(1);
        }
    }

    /// `(v1, v2)`.
    pub fn resultat(&self) -> (u32, u32) {
        (self.v1, self.v2)
    }
}

pub fn hex(crc: u32) -> String {
    format!("{crc:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trames(mots: &[u32]) -> Vec<u8> {
        mots.iter().flat_map(|m| m.to_le_bytes()).collect()
    }

    /// Piste du milieu : 1×1 + 2×2 + 3×3 = 14, sans dépassement v1 = v2.
    #[test]
    fn la_somme_ponderee_par_la_position() {
        let mut c = CalculAccurateRip::new(3, false, false);
        c.ajouter(&trames(&[1, 2, 3]));
        assert_eq!(c.resultat(), (14, 14));
    }

    /// Le dépassement sépare v1 et v2 : 0xFFFFFFFF × 2 = 0x1_FFFF_FFFE ;
    /// v1 garde la moitié basse, v2 y ajoute la haute.
    #[test]
    fn v2_ajoute_la_moitie_haute_du_produit() {
        let mut c = CalculAccurateRip::new(2, false, false);
        c.ajouter(&trames(&[0, 0xFFFF_FFFF]));
        assert_eq!(c.resultat(), (0xFFFF_FFFE, 0xFFFF_FFFF));
    }

    /// Première piste : les positions 1 à 2 939 ne comptent pas, la 2 940
    /// oui. Dernière piste : les 2 940 dernières positions ne comptent pas.
    #[test]
    fn les_bords_du_disque_sont_sautes() {
        let n = 10_000u32;
        let uns = trames(&vec![1u32; n as usize]);
        let somme = |a: u32, b: u32| (a..=b).fold(0u32, |s, p| s.wrapping_add(p));

        let mut c = CalculAccurateRip::new(n, true, false);
        c.ajouter(&uns);
        assert_eq!(c.resultat().0, somme(2_940, n));

        let mut c = CalculAccurateRip::new(n, false, true);
        c.ajouter(&uns);
        assert_eq!(c.resultat().0, somme(1, n - 2_940));

        // Contre-épreuve : sans saut, la somme entière.
        let mut c = CalculAccurateRip::new(n, false, false);
        c.ajouter(&uns);
        assert_eq!(c.resultat().0, somme(1, n));
    }

    /// Ajouter par morceaux ne change rien : la position suit les blocs.
    #[test]
    fn le_calcul_par_blocs_egale_le_calcul_d_un_seul_tenant() {
        let mots: Vec<u32> = (0..5_000u32)
            .map(|i| i.wrapping_mul(2_654_435_761))
            .collect();
        let tout = trames(&mots);
        let mut a = CalculAccurateRip::new(5_000, true, true);
        a.ajouter(&tout);
        let mut b = CalculAccurateRip::new(5_000, true, true);
        for morceau in tout.chunks(588 * 4) {
            b.ajouter(morceau);
        }
        assert_eq!(a.resultat(), b.resultat());
    }
}
