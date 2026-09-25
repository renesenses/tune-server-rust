//! Le PCM d'une piste, lu secteur par secteur au rythme où on le demande.
//!
//! `FluxPiste` est un `std::io::Read` : l'hôte le pompe dans une session de
//! flux (`tune_core::source_pcm`). Il ne rend QUE les secteurs de sa plage —
//! pas un octet de la piste voisine — et il ne s'arrête pas sur une rayure :
//!
//! 1. un bloc qui échoue est relu jusqu'à `ESSAIS` fois ;
//! 2. s'il échoue encore, il est relu secteur par secteur, chacun jusqu'à
//!    `ESSAIS` fois ;
//! 3. un secteur toujours illisible est remplacé par du SILENCE, et la plage
//!    perdue est journalisée (`cd_secteurs_remplaces_par_du_silence`).
//!
//! L'éjection, elle, n'est pas une rayure : `ErreurCd::AucunDisque` arrête le
//! flux par une erreur, que l'hôte traite comme une fin anormale.

use std::io::Read;
use std::sync::Arc;

use crate::lecteur::{ErreurCd, LecteurDisque};
use crate::toc::OCTETS_PAR_SECTEUR;

/// Secteurs lus par appel au lecteur : 24 secteurs = 0,32 s de musique,
/// 56 448 octets. Sous la limite de `CDROMREADAUDIO` des pilotes courants.
pub const SECTEURS_PAR_BLOC: u32 = 24;
/// Tentatives par bloc, puis par secteur, avant le silence.
pub const ESSAIS: u32 = 3;

pub struct FluxPiste {
    lecteur: Arc<dyn LecteurDisque>,
    /// Prochain secteur à lire (LBA).
    prochain: u32,
    /// Premier secteur HORS de la plage (LBA) : jamais lu.
    fin: u32,
    tampon: Vec<u8>,
    lu_dans_tampon: usize,
    /// Secteurs remplacés par du silence depuis l'ouverture.
    pub secteurs_perdus: u32,
}

impl FluxPiste {
    /// Le flux des secteurs `[debut, fin)`.
    pub fn new(lecteur: Arc<dyn LecteurDisque>, debut: u32, fin: u32) -> Self {
        Self {
            lecteur,
            prochain: debut,
            fin: fin.max(debut),
            tampon: Vec::new(),
            lu_dans_tampon: 0,
            secteurs_perdus: 0,
        }
    }

    pub fn octets_restants(&self) -> u64 {
        (self.fin - self.prochain) as u64 * OCTETS_PAR_SECTEUR as u64
            + (self.tampon.len() - self.lu_dans_tampon) as u64
    }

    fn remplir(&mut self) -> std::io::Result<()> {
        let n = (self.fin - self.prochain).min(SECTEURS_PAR_BLOC);
        let lba = self.prochain;
        let mut bloc = vec![0u8; n as usize * OCTETS_PAR_SECTEUR];
        let mut derniere = None;
        for _ in 0..ESSAIS {
            match self.lecteur.lire_secteurs(lba, n, &mut bloc) {
                Ok(()) => {
                    derniere = None;
                    break;
                }
                Err(ErreurCd::AucunDisque) => return Err(ejection()),
                Err(e) => derniere = Some(e),
            }
        }
        if derniere.is_some() {
            self.relire_secteur_par_secteur(lba, n, &mut bloc)?;
        }
        self.prochain += n;
        self.tampon = bloc;
        self.lu_dans_tampon = 0;
        Ok(())
    }

    fn relire_secteur_par_secteur(
        &mut self,
        lba: u32,
        n: u32,
        bloc: &mut [u8],
    ) -> std::io::Result<()> {
        let mut perte: Option<(u32, u32)> = None;
        for s in lba..lba + n {
            let i = (s - lba) as usize * OCTETS_PAR_SECTEUR;
            let tranche = &mut bloc[i..i + OCTETS_PAR_SECTEUR];
            let mut lu = false;
            for _ in 0..ESSAIS {
                match self.lecteur.lire_secteurs(s, 1, tranche) {
                    Ok(()) => {
                        lu = true;
                        break;
                    }
                    Err(ErreurCd::AucunDisque) => return Err(ejection()),
                    Err(_) => {}
                }
            }
            if !lu {
                tranche.fill(0);
                self.secteurs_perdus += 1;
                perte = Some(match perte {
                    Some((d, _)) => (d, s),
                    None => (s, s),
                });
            } else if let Some((d, f)) = perte.take() {
                journaliser_perte(d, f);
            }
        }
        if let Some((d, f)) = perte {
            journaliser_perte(d, f);
        }
        Ok(())
    }
}

fn ejection() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "le disque a été éjecté")
}

fn journaliser_perte(debut: u32, fin: u32) {
    tracing::warn!(
        premier_secteur = debut,
        dernier_secteur = fin,
        secteurs = fin - debut + 1,
        "cd_secteurs_remplaces_par_du_silence"
    );
}

impl Read for FluxPiste {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.lu_dans_tampon == self.tampon.len() {
            if self.prochain >= self.fin {
                return Ok(0);
            }
            self.remplir()?;
        }
        let dispo = &self.tampon[self.lu_dans_tampon..];
        let n = dispo.len().min(buf.len());
        buf[..n].copy_from_slice(&dispo[..n]);
        self.lu_dans_tampon += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simule::{LecteurSimule, contenu_des_secteurs};
    use crate::toc::{PisteToc, Toc};

    fn lecteur() -> Arc<LecteurSimule> {
        Arc::new(LecteurSimule::new(
            Toc::nouvelle(
                vec![PisteToc {
                    numero: 1,
                    debut: 0,
                    audio: true,
                }],
                500,
            )
            .unwrap(),
        ))
    }

    fn tout_lire(f: &mut FluxPiste) -> std::io::Result<Vec<u8>> {
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        Ok(v)
    }

    #[test]
    fn le_flux_rend_exactement_sa_plage_de_secteurs() {
        let l = lecteur();
        let mut f = FluxPiste::new(l, 10, 110);
        assert_eq!(f.octets_restants(), 100 * 2_352);
        assert_eq!(tout_lire(&mut f).unwrap(), contenu_des_secteurs(10, 100));
    }

    /// Témoin 6 — une erreur de secteur est rejouée, puis remplacée par du
    /// silence si elle persiste. La lecture ne s'arrête pas.
    #[test]
    fn une_erreur_de_secteur_est_rejouee_puis_remplacee_par_du_silence() {
        let l = lecteur();
        // Le secteur 30 échoue deux fois puis se lit : rejoué, pas perdu.
        l.faire_echouer(30, 2);
        // Le secteur 50 échoue toujours : silence, et seulement lui.
        l.faire_echouer(50, u32::MAX);
        let mut f = FluxPiste::new(l.clone(), 20, 70);
        let v = tout_lire(&mut f).unwrap();
        assert_eq!(v.len(), 50 * 2_352);
        let mut attendu = contenu_des_secteurs(20, 50);
        let i = (50 - 20) * 2_352;
        attendu[i..i + 2_352].fill(0);
        assert_eq!(v, attendu, "seul le secteur 50 devient du silence");
        assert_eq!(f.secteurs_perdus, 1);
        assert!(l.tentatives(30) >= 2, "le secteur 30 a été rejoué");
        // Bloc : ESSAIS tentatives, puis secteur seul : ESSAIS tentatives.
        assert_eq!(l.tentatives(50), 2 * ESSAIS);
    }

    #[test]
    fn l_ejection_arrete_le_flux_par_une_erreur_et_non_par_du_silence() {
        let l = lecteur();
        l.ejecter_apres(2);
        let mut f = FluxPiste::new(l, 0, 200);
        let e = tout_lire(&mut f).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
        assert_eq!(f.secteurs_perdus, 0);
    }
}
