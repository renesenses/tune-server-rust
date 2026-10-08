//! La lecture d'un bloc de secteurs pour l'EXTRACTION (#2466).
//!
//! Plus exigeante que celle de la lecture vers une zone (`flux.rs`), qui ne
//! peut pas attendre : ici, un bloc douteux est relu jusqu'à ce que deux
//! lectures concordent, octet pour octet.
//!
//! 1. Le bloc est lu. Mode `doute` (défaut) : une lecture sans erreur est
//!    acceptée telle quelle. Mode `toujours` : elle doit être confirmée par
//!    une seconde lecture identique.
//! 2. En cas d'erreur (ou en mode `toujours`), le bloc est relu jusqu'à
//!    [`LECTURES_MAX`] fois ; il est accepté dès que deux lectures réussies
//!    sont IDENTIQUES.
//! 3. Sans concordance, chaque secteur du bloc passe par la même règle,
//!    seul.
//! 4. Un secteur sans deux lectures concordantes est remplacé par du
//!    silence, et compté (`secteurs_illisibles`).
//!
//! Limite connue : un lecteur qui sert la relecture depuis son cache rend
//! deux fois les mêmes octets. Contourner le cache (lire ailleurs entre deux
//! relectures, ou la commande FUA) est pour la version complète.
//!
//! L'éjection (`ErreurCd::AucunDisque`) n'est pas une rayure : elle arrête
//! tout, sans relecture.

use serde::{Deserialize, Serialize};

use crate::lecteur::{ErreurCd, LecteurDisque};
use crate::toc::OCTETS_PAR_SECTEUR;

/// Secteurs par lecture : la taille de bloc de la lecture vers une zone.
pub const SECTEURS_PAR_BLOC: u32 = crate::flux::SECTEURS_PAR_BLOC;
/// Lectures au plus, par bloc puis par secteur, pour obtenir deux lectures
/// concordantes.
pub const LECTURES_MAX: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// Relire seulement un bloc qui a donné une erreur.
    #[default]
    Doute,
    /// Confirmer CHAQUE bloc par une seconde lecture (deux fois plus long).
    Toujours,
}

/// Ce que la lecture a coûté, cumulé sur une piste.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Bilan {
    /// Lectures faites en plus de la première de chaque bloc.
    pub lectures_supplementaires: u32,
    /// Secteurs remplacés par du silence.
    pub secteurs_illisibles: u32,
}

/// Lit `n` secteurs à partir de `lba`. Rend toujours `n × 2 352` octets,
/// sauf si le disque a disparu.
pub fn lire_bloc(
    lecteur: &dyn LecteurDisque,
    lba: u32,
    n: u32,
    verification: Verification,
    bilan: &mut Bilan,
) -> Result<Vec<u8>, ErreurCd> {
    let mut premier = vec![0u8; n as usize * OCTETS_PAR_SECTEUR];
    let premiere_lecture = match lecteur.lire_secteurs(lba, n, &mut premier) {
        Ok(()) if verification == Verification::Doute => return Ok(premier),
        Ok(()) => Some(premier),
        Err(ErreurCd::AucunDisque) => return Err(ErreurCd::AucunDisque),
        Err(_) => None,
    };
    if let Some(v) = concordance(lecteur, lba, n, premiere_lecture, LECTURES_MAX - 1, bilan)? {
        return Ok(v);
    }
    // Le bloc entier ne concorde pas : secteur par secteur.
    let mut sortie = Vec::with_capacity(n as usize * OCTETS_PAR_SECTEUR);
    for s in lba..lba + n {
        match concordance(lecteur, s, 1, None, LECTURES_MAX, bilan)? {
            Some(v) => sortie.extend_from_slice(&v),
            None => {
                bilan.secteurs_illisibles += 1;
                tracing::warn!(
                    lba = s,
                    "cd_extraction_secteur_illisible_remplace_par_du_silence"
                );
                sortie.resize(sortie.len() + OCTETS_PAR_SECTEUR, 0);
            }
        }
    }
    Ok(sortie)
}

/// Relit au plus `essais` fois ; rend la première lecture qui en égale une
/// précédente (y compris `deja`), `None` sans concordance.
fn concordance(
    lecteur: &dyn LecteurDisque,
    lba: u32,
    n: u32,
    deja: Option<Vec<u8>>,
    essais: u32,
    bilan: &mut Bilan,
) -> Result<Option<Vec<u8>>, ErreurCd> {
    let mut reussies: Vec<Vec<u8>> = deja.into_iter().collect();
    for _ in 0..essais {
        let mut b = vec![0u8; n as usize * OCTETS_PAR_SECTEUR];
        bilan.lectures_supplementaires += 1;
        match lecteur.lire_secteurs(lba, n, &mut b) {
            Ok(()) => {
                if reussies.contains(&b) {
                    return Ok(Some(b));
                }
                reussies.push(b);
            }
            Err(ErreurCd::AucunDisque) => return Err(ErreurCd::AucunDisque),
            Err(_) => {}
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discid::tests::toc_du_vecteur;
    use crate::simule::{LecteurSimule, contenu_des_secteurs};

    fn lecteur() -> LecteurSimule {
        LecteurSimule::new(toc_du_vecteur())
    }

    #[test]
    fn un_bloc_sain_est_lu_une_seule_fois_en_mode_doute() {
        let l = lecteur();
        let mut b = Bilan::default();
        let v = lire_bloc(&l, 1_000, 24, Verification::Doute, &mut b).unwrap();
        assert_eq!(v, contenu_des_secteurs(1_000, 24));
        assert_eq!(b, Bilan::default());
        assert_eq!(l.appels(), 1);
    }

    /// Une erreur, puis une lecture FAUSSE sans erreur, puis des lectures
    /// justes : la lecture fausse n'est pas retenue, faute d'une seconde
    /// lecture qui la confirme. Contre-épreuve : accepter la première
    /// lecture réussie après l'erreur rendrait l'octet faux.
    #[test]
    fn apres_une_erreur_seules_deux_lectures_concordantes_sont_acceptees() {
        let l = lecteur();
        l.faire_echouer(1_005, 1);
        l.corrompre(1_005, 1);
        let mut b = Bilan::default();
        let v = lire_bloc(&l, 1_000, 24, Verification::Doute, &mut b).unwrap();
        assert_eq!(v, contenu_des_secteurs(1_000, 24), "octets justes");
        assert_eq!(b.secteurs_illisibles, 0);
        // Erreur, fausse, juste, juste : trois lectures de plus.
        assert_eq!(b.lectures_supplementaires, 3);
    }

    /// Mode `toujours` : une corruption SILENCIEUSE (aucune erreur) est vue,
    /// parce que la première lecture doit être confirmée. Contre-épreuve :
    /// en mode `doute`, la même corruption passe.
    #[test]
    fn le_mode_toujours_voit_une_corruption_silencieuse() {
        let l = lecteur();
        l.corrompre(1_010, 1);
        let mut b = Bilan::default();
        let v = lire_bloc(&l, 1_000, 24, Verification::Toujours, &mut b).unwrap();
        assert_eq!(v, contenu_des_secteurs(1_000, 24));
        assert_eq!(b.lectures_supplementaires, 2);

        let l = lecteur();
        l.corrompre(1_010, 1);
        let v = lire_bloc(&l, 1_000, 24, Verification::Doute, &mut Bilan::default()).unwrap();
        assert_ne!(v, contenu_des_secteurs(1_000, 24), "contre-épreuve");
    }

    /// Un secteur qui échoue toujours : le bloc passe secteur par secteur,
    /// les autres secteurs sont justes, lui seul devient du silence.
    #[test]
    fn un_secteur_illisible_devient_du_silence_et_est_compte() {
        let l = lecteur();
        l.faire_echouer(1_003, u32::MAX);
        let mut b = Bilan::default();
        let v = lire_bloc(&l, 1_000, 24, Verification::Doute, &mut b).unwrap();
        assert_eq!(b.secteurs_illisibles, 1);
        let mut attendu = contenu_des_secteurs(1_000, 24);
        attendu[3 * OCTETS_PAR_SECTEUR..4 * OCTETS_PAR_SECTEUR].fill(0);
        assert_eq!(v, attendu);
    }

    #[test]
    fn l_ejection_arrete_sans_relire() {
        let l = lecteur();
        l.ejecter();
        let mut b = Bilan::default();
        assert_eq!(
            lire_bloc(&l, 1_000, 24, Verification::Toujours, &mut b),
            Err(ErreurCd::AucunDisque)
        );
        assert_eq!(b.lectures_supplementaires, 0);
    }
}
