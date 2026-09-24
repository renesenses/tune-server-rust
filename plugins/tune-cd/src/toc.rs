//! La table des pistes (TOC) d'un CD audio, et l'arithmétique des secteurs.
//!
//! Tout ici est pur : aucune ligne ne parle au noyau.

use serde::Serialize;

/// Un secteur audio : 2 352 octets = 588 trames stéréo 16 bits.
pub const OCTETS_PAR_SECTEUR: usize = 2_352;
/// Trames stéréo par secteur.
pub const TRAMES_PAR_SECTEUR: u64 = 588;
/// Le CD tourne à 75 secteurs par seconde de musique.
pub const SECTEURS_PAR_SECONDE: u64 = 75;
/// Décalage entre une adresse LBA et l'offset « absolu » que MusicBrainz
/// emploie : les 2 secondes de pré-gap initial.
pub const PREGAP: u32 = 150;
/// Sur un CD « enrichi » (CD-Extra), la session de données commence
/// 11 400 secteurs après la fin de la dernière piste audio : fin de session
/// (6 750) + début de la suivante (4 500) + pré-gap (150).
pub const ECART_SESSION_DONNEES: u32 = 11_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PisteToc {
    /// Numéro de la piste sur le disque (1 à 99).
    pub numero: u8,
    /// Premier secteur, en LBA (0 = début du programme).
    pub debut: u32,
    /// `false` pour une piste de données (bit « data » du champ de contrôle).
    pub audio: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Toc {
    pub premiere: u8,
    pub derniere: u8,
    pub pistes: Vec<PisteToc>,
    /// Le « lead-out » : premier secteur APRÈS le programme, en LBA.
    pub fin: u32,
}

impl Toc {
    /// Construit une TOC en vérifiant sa cohérence : numéros consécutifs,
    /// débuts strictement croissants, fin après le dernier début, au moins une
    /// piste audio.
    pub fn nouvelle(pistes: Vec<PisteToc>, fin: u32) -> Result<Toc, String> {
        let (Some(p), Some(d)) = (pistes.first(), pistes.last()) else {
            return Err("TOC vide".into());
        };
        let (premiere, derniere) = (p.numero, d.numero);
        for (i, piste) in pistes.iter().enumerate() {
            if piste.numero as usize != premiere as usize + i {
                return Err(format!(
                    "TOC : numéros non consécutifs à la piste {}",
                    piste.numero
                ));
            }
            if i > 0 && piste.debut <= pistes[i - 1].debut {
                return Err(format!(
                    "TOC : la piste {} ne suit pas la précédente",
                    piste.numero
                ));
            }
        }
        if fin <= d.debut {
            return Err("TOC : fin avant la dernière piste".into());
        }
        if !pistes.iter().any(|p| p.audio) {
            return Err("ce disque ne porte aucune piste audio".into());
        }
        Ok(Toc {
            premiere,
            derniere,
            pistes,
            fin,
        })
    }

    pub fn piste(&self, numero: u8) -> Option<&PisteToc> {
        self.pistes.iter().find(|p| p.numero == numero)
    }

    pub fn pistes_audio(&self) -> impl Iterator<Item = &PisteToc> {
        self.pistes.iter().filter(|p| p.audio)
    }

    /// Premier secteur APRÈS la piste `numero` : le début de la suivante, ou
    /// la fin du disque. Une piste audio suivie d'une piste de DONNÉES (CD
    /// enrichi) s'arrête `ECART_SESSION_DONNEES` secteurs avant celle-ci —
    /// sans quoi on servirait la fin de session comme de la musique.
    pub fn fin_de_piste(&self, numero: u8) -> Option<u32> {
        let i = self.pistes.iter().position(|p| p.numero == numero)?;
        Some(match self.pistes.get(i + 1) {
            Some(suivante) if self.pistes[i].audio && !suivante.audio => suivante
                .debut
                .saturating_sub(ECART_SESSION_DONNEES)
                .max(self.pistes[i].debut),
            Some(suivante) => suivante.debut,
            None => self.fin,
        })
    }

    /// Longueur de la piste, en secteurs.
    pub fn secteurs(&self, numero: u8) -> Option<u32> {
        let debut = self.piste(numero)?.debut;
        Some(self.fin_de_piste(numero)? - debut)
    }

    /// Durée de la piste, en millisecondes (arrondie vers le bas).
    pub fn duree_ms(&self, numero: u8) -> Option<u64> {
        self.secteurs(numero).map(duree_ms_de_secteurs)
    }

    /// La dernière piste AUDIO et la fin du programme audio : ce que
    /// l'identifiant MusicBrainz compte (une piste de données finale en est
    /// exclue, son début moins l'écart de session tient lieu de fin).
    pub fn fin_audio(&self) -> (u8, u32) {
        let derniere_audio = self
            .pistes_audio()
            .last()
            .map(|p| p.numero)
            .unwrap_or(self.derniere);
        let fin = self.fin_de_piste(derniere_audio).unwrap_or(self.fin);
        (derniere_audio, fin)
    }
}

pub fn duree_ms_de_secteurs(secteurs: u32) -> u64 {
    secteurs as u64 * 1_000 / SECTEURS_PAR_SECONDE
}

/// Le secteur (relatif au début de la piste) où tombe une position en
/// millisecondes : arrondi vers le bas, pour ne jamais sauter de musique.
pub fn secteur_de_position(position_ms: u64) -> u64 {
    position_ms * SECTEURS_PAR_SECONDE / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(numero: u8, debut: u32) -> PisteToc {
        PisteToc {
            numero,
            debut,
            audio: true,
        }
    }

    /// Témoin 1 — la TOC donne les bonnes pistes et les bonnes durées.
    #[test]
    fn la_toc_donne_les_bonnes_pistes_et_les_bonnes_durees() {
        // Les offsets du vecteur libdiscid (moins le pré-gap de 150).
        let toc = Toc::nouvelle(vec![p(1, 0), p(2, 18_751), p(3, 39_588)], 59_407).unwrap();
        assert_eq!((toc.premiere, toc.derniere), (1, 3));
        assert_eq!(toc.secteurs(1), Some(18_751));
        assert_eq!(toc.secteurs(2), Some(20_837));
        assert_eq!(toc.secteurs(3), Some(19_819));
        // 18 751 secteurs / 75 = 250,013 s.
        assert_eq!(toc.duree_ms(1), Some(250_013));
        assert_eq!(toc.duree_ms(2), Some(277_826));
        assert_eq!(toc.duree_ms(4), None);
    }

    #[test]
    fn une_piste_de_donnees_finale_ne_mange_pas_la_derniere_piste_audio() {
        let toc = Toc::nouvelle(
            vec![
                p(1, 0),
                p(2, 20_000),
                PisteToc {
                    numero: 3,
                    debut: 60_000,
                    audio: false,
                },
            ],
            90_000,
        )
        .unwrap();
        assert_eq!(toc.secteurs(2), Some(60_000 - 11_400 - 20_000));
        assert_eq!(toc.fin_audio(), (2, 48_600));
        assert_eq!(toc.pistes_audio().count(), 2);
    }

    #[test]
    fn une_toc_incoherente_est_refusee() {
        assert!(Toc::nouvelle(vec![], 10).is_err());
        assert!(Toc::nouvelle(vec![p(1, 0), p(3, 10)], 20).is_err());
        assert!(Toc::nouvelle(vec![p(1, 10), p(2, 10)], 20).is_err());
        assert!(Toc::nouvelle(vec![p(1, 0)], 0).is_err());
    }

    #[test]
    fn la_position_tombe_sur_le_secteur_de_son_instant() {
        assert_eq!(secteur_de_position(0), 0);
        assert_eq!(secteur_de_position(1_000), 75);
        // 13,33 ms par secteur : 26 ms tombent dans le secteur 1, pas le 2.
        assert_eq!(secteur_de_position(26), 1);
        assert_eq!(secteur_de_position(27), 2);
    }
}
