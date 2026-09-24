//! Un lecteur SIMULÉ : une TOC et des secteurs en mémoire.
//!
//! Il sert aux témoins (Shrek n'a pas de lecteur), et il sait simuler les
//! deux accidents qui comptent : un secteur qui échoue quelques fois (ou
//! toujours), et l'éjection du disque au bout d'un nombre de lectures.
//!
//! Chaque secteur est rempli d'un motif qui DIT son numéro : un octet de la
//! piste voisine ne peut pas se faire passer pour un octet de la bonne piste.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::lecteur::{ErreurCd, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, Toc};

#[derive(Default)]
struct Etat {
    /// Secteur → nombre d'échecs restants (`u32::MAX` = toujours).
    echecs: HashMap<u32, u32>,
    /// Tentatives de lecture par secteur, pour prouver la reprise.
    tentatives: HashMap<u32, u32>,
    /// Éjection après ce nombre d'appels à `lire_secteurs`.
    ejection_apres: Option<u32>,
    appels: u32,
    ejecte: bool,
}

pub struct LecteurSimule {
    toc: Toc,
    etat: Mutex<Etat>,
}

/// L'octet `i` du secteur `lba` : les 4 premiers octets de chaque trame de
/// 4 portent le numéro du secteur, puis la position dans le secteur.
pub fn octet_du_secteur(lba: u32, i: usize) -> u8 {
    let mot = lba.wrapping_mul(2_654_435_761) ^ (i as u32 / 4);
    mot.to_le_bytes()[i % 4]
}

pub fn contenu_des_secteurs(lba: u32, nombre: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(nombre as usize * OCTETS_PAR_SECTEUR);
    for s in lba..lba + nombre {
        v.extend((0..OCTETS_PAR_SECTEUR).map(|i| octet_du_secteur(s, i)));
    }
    v
}

impl LecteurSimule {
    pub fn new(toc: Toc) -> Self {
        Self {
            toc,
            etat: Mutex::new(Etat::default()),
        }
    }

    /// Le secteur `lba` échouera `fois` fois avant de se laisser lire.
    pub fn faire_echouer(&self, lba: u32, fois: u32) {
        self.etat.lock().unwrap().echecs.insert(lba, fois);
    }

    /// Le disque sera éjecté après `appels` lectures de secteurs.
    pub fn ejecter_apres(&self, appels: u32) {
        self.etat.lock().unwrap().ejection_apres = Some(appels);
    }

    pub fn ejecter(&self) {
        self.etat.lock().unwrap().ejecte = true;
    }

    pub fn tentatives(&self, lba: u32) -> u32 {
        self.etat
            .lock()
            .unwrap()
            .tentatives
            .get(&lba)
            .copied()
            .unwrap_or(0)
    }
}

impl LecteurDisque for LecteurSimule {
    fn chemin(&self) -> String {
        "simulé".into()
    }

    fn presence(&self) -> Presence {
        if self.etat.lock().unwrap().ejecte {
            Presence::Vide
        } else {
            Presence::Disque
        }
    }

    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        if self.etat.lock().unwrap().ejecte {
            return Err(ErreurCd::AucunDisque);
        }
        Ok(self.toc.clone())
    }

    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        let mut e = self.etat.lock().unwrap();
        e.appels += 1;
        if e.ejection_apres.is_some_and(|n| e.appels > n) {
            e.ejecte = true;
        }
        if e.ejecte {
            return Err(ErreurCd::AucunDisque);
        }
        if lba + nombre > self.toc.fin || sortie.len() != nombre as usize * OCTETS_PAR_SECTEUR {
            return Err(ErreurCd::Autre("lecture hors du disque".into()));
        }
        for s in lba..lba + nombre {
            *e.tentatives.entry(s).or_default() += 1;
        }
        for s in lba..lba + nombre {
            if let Some(reste) = e.echecs.get_mut(&s)
                && *reste > 0
            {
                if *reste != u32::MAX {
                    *reste -= 1;
                }
                return Err(ErreurCd::Lecture {
                    lba: s,
                    raison: "erreur simulée".into(),
                });
            }
        }
        sortie.copy_from_slice(&contenu_des_secteurs(lba, nombre));
        Ok(())
    }
}
