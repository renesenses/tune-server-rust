//! Radio de fin de file (forum, fil 2694, Ilan, 12/11/2026 ; décision de
//! Bertrand du 13/11) : « continuer après la dernière chanson avec une radio,
//! comme Roon ».
//!
//! L'enchaînement lui-même existe depuis longtemps (`poller::fin_de_piste`,
//! réglage de zone `autoplay_enabled`, #2271). Ce module porte ce qui lui
//! manquait pour être activé par défaut sans surprendre :
//!
//! - **la marque « ajouté par la radio »** : les lignes de file
//!   (`queue_items.id`) que la fin de file a ajoutées, rangées dans le
//!   réglage de zone `zone_{id}_radio_fin_de_file` (survit au redémarrage,
//!   part avec la zone comme les autres `zone_{id}_*`). La route
//!   `GET /zones/{id}/queue` en tire `added_by_radio` pour le badge. Les
//!   identifiants de ligne ne sont jamais réutilisés (AUTOINCREMENT,
//!   BIGSERIAL) : une ligne remplacée perd sa marque, elle n'en hérite
//!   jamais d'une autre ;
//! - **la fin VOULUE** : « Vider la suite » (on garde la piste en cours et
//!   on retire le reste) ou une file posée par le réveil disent « arrête-toi
//!   après ça ». On photographie alors la file ; si la fin de file arrive
//!   sur EXACTEMENT les mêmes lignes, la radio ne s'en mêle pas. Une file
//!   qui a changé depuis (ajout, nouvelle lecture) efface la consigne.
//!   Gardée en mémoire seulement : après un redémarrage, la lecture est de
//!   toute façon arrêtée ;
//! - **la fin sur ERREUR** (relecture de #8656) : une piste qui échoue
//!   (`poller::tick`, fichier abîmé) passe aussi par la fin de piste ; la
//!   radio ne part que sur une fin NATURELLE. Et un lot dont aucun titre n'a
//!   été joué en entier n'en appelle pas un autre : pas de boucle sans fin
//!   sur des titres qui échouent tous ;
//! - **les groupes** : seule la meneuse lance la radio, le relais de groupe
//!   la propage aux suiveuses ;
//! - **la file bornée** : au-delà de [`GARDER_JOUEES`] lignes de radio déjà
//!   jouées, les plus anciennes quittent la file ;
//! - les marques ne voyagent pas dans les sauvegardes de configuration
//!   ([`cle_de_marques`]).

use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

/// Au plus autant de marques gardées par zone : la file d'une radio qui
/// tourne toute la journée ne fait pas grossir le réglage sans fin.
pub const MARQUES_MAX: usize = 500;

/// La clé du réglage qui porte les marques d'une zone.
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_radio_fin_de_file")
}

/// Les identifiants des lignes de la file de la zone, dans l'ordre.
pub fn lignes_de_la_file(db: &Arc<dyn DbBackend>, zone_id: i64) -> Vec<i64> {
    PlayQueueRepo::with_backend(db.clone())
        .get_ordered(zone_id)
        .map(|rows| rows.into_iter().map(|r| r.id).collect())
        .unwrap_or_default()
}

/// Les lignes de la file que la radio de fin de file a ajoutées.
pub fn lignes_ajoutees_par_la_radio(db: &Arc<dyn DbBackend>, zone_id: i64) -> HashSet<i64> {
    SettingsRepo::with_backend(db.clone())
        .get(&cle(zone_id))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Vec<i64>>(&v).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Marque comme « ajoutées par la radio » les `ajoutees` DERNIÈRES lignes
/// nouvelles de la file (absentes de `avant`). Un titre que l'auditeur a
/// enfilé pendant l'appel au service arrive AVANT le lot de la radio : il
/// n'est pas marqué. Les marques des lignes sorties de la file sont oubliées
/// au passage. Rend les lignes marquées.
pub fn marquer_les_ajouts(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    avant: &[i64],
    ajoutees: usize,
) -> Vec<i64> {
    if ajoutees == 0 {
        return Vec::new();
    }
    let maintenant = lignes_de_la_file(db, zone_id);
    let avant: HashSet<i64> = avant.iter().copied().collect();
    let nouvelles: Vec<i64> = maintenant
        .iter()
        .copied()
        .filter(|id| !avant.contains(id))
        .collect();
    let lot: Vec<i64> = nouvelles[nouvelles.len().saturating_sub(ajoutees)..].to_vec();
    if lot.is_empty() {
        return lot;
    }
    let deja = lignes_ajoutees_par_la_radio(db, zone_id);
    let mut gardees: Vec<i64> = maintenant
        .iter()
        .copied()
        .filter(|id| deja.contains(id) || lot.contains(id))
        .collect();
    if gardees.len() > MARQUES_MAX {
        gardees.drain(..gardees.len() - MARQUES_MAX);
    }
    let valeur = serde_json::to_string(&gardees).unwrap_or_else(|_| "[]".into());
    if let Err(e) = SettingsRepo::with_backend(db.clone()).set(&cle(zone_id), &valeur) {
        tracing::warn!(zone_id, error = %e, "radio_fin_de_file_marques_non_ecrites");
    }
    lot
}

/// Les lignes de radio DÉJÀ JOUÉES (avant `position_courante`) à retirer de
/// la file pour n'en garder que les `garder` plus récentes.
pub fn lignes_de_radio_a_purger(
    file: &[i64],
    marques: &HashSet<i64>,
    position_courante: i64,
    garder: usize,
) -> Vec<i64> {
    let jouees: Vec<i64> = file
        .iter()
        .take(position_courante.max(0) as usize)
        .copied()
        .filter(|id| marques.contains(id))
        .collect();
    jouees[..jouees.len().saturating_sub(garder)].to_vec()
}

/// Lignes de radio déjà jouées gardées dans la file (relecture de #8656).
pub const GARDER_JOUEES: usize = 50;

/// La clé est-elle celle des marques d'une zone ? Exclue des sauvegardes et
/// restaurations de configuration : elle désigne des lignes de file de CETTE
/// base, rien sur une autre machine.
pub fn cle_de_marques(cle: &str) -> bool {
    cle.strip_prefix("zone_")
        .and_then(|reste| reste.strip_suffix("_radio_fin_de_file"))
        .is_some_and(|id| {
            id == "{id}" || (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// La zone SUIT-elle une autre dans un groupe (`zone_groups`) ? La meneuse
/// est `leader_id`, sinon `leader_zone_id`, sinon la première zone — la règle
/// du relais de groupe (#6155).
pub fn zone_suiveuse(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    let groupes: Vec<serde_json::Value> = SettingsRepo::with_backend(db.clone())
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    groupes.iter().any(|g| {
        let zones: Vec<i64> = g["zone_ids"]
            .as_array()
            .map(|ids| ids.iter().filter_map(serde_json::Value::as_i64).collect())
            .unwrap_or_default();
        let meneuse = g["leader_id"]
            .as_i64()
            .or_else(|| g["leader_zone_id"].as_i64())
            .or_else(|| zones.first().copied());
        zones.contains(&zone_id) && meneuse != Some(zone_id)
    })
}

static FINS_SUR_ERREUR: LazyLock<Mutex<HashSet<i64>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// La piste qui s'achève a ÉCHOUÉ (erreur de lecture, fichier abîmé) : la
/// fin de piste qui suit n'est pas une fin naturelle.
pub fn noter_fin_sur_erreur(zone_id: i64) {
    if let Ok(mut m) = FINS_SUR_ERREUR.lock() {
        m.insert(zone_id);
    }
}

/// Consomme la consigne de [`noter_fin_sur_erreur`].
pub fn prendre_fin_sur_erreur(zone_id: i64) -> bool {
    FINS_SUR_ERREUR
        .lock()
        .map(|mut m| m.remove(&zone_id))
        .unwrap_or(false)
}

/// Le dernier lot de la radio d'une zone, et si l'un de ses titres a été
/// joué en entier.
static DERNIERS_LOTS: LazyLock<Mutex<HashMap<i64, (Vec<i64>, bool)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Retient le lot que la radio vient d'ajouter.
pub fn noter_lot(zone_id: i64, lignes: Vec<i64>) {
    if let Ok(mut m) = DERNIERS_LOTS.lock() {
        m.insert(zone_id, (lignes, false));
    }
}

/// La zone a-t-elle un lot de radio en cours ? (évite de relire la file à
/// chaque fin de piste quand la radio n'a rien ajouté).
pub fn a_un_lot(zone_id: i64) -> bool {
    DERNIERS_LOTS
        .lock()
        .map(|m| m.contains_key(&zone_id))
        .unwrap_or(false)
}

/// La ligne `ligne` vient d'être jouée en entier : si elle est du dernier
/// lot, le lot a « servi ».
pub fn noter_ligne_jouee(zone_id: i64, ligne: i64) {
    if let Ok(mut m) = DERNIERS_LOTS.lock()
        && let Some((lot, joue)) = m.get_mut(&zone_id)
        && lot.contains(&ligne)
    {
        *joue = true;
    }
}

/// La fin de file arrive sur une ligne du dernier lot, et AUCUN titre de ce
/// lot n'a été joué en entier : la radio ne relance pas.
pub fn lot_muet(zone_id: i64, ligne_courante: Option<i64>) -> bool {
    let Some(ligne) = ligne_courante else {
        return false;
    };
    DERNIERS_LOTS
        .lock()
        .ok()
        .and_then(|m| {
            m.get(&zone_id)
                .map(|(lot, joue)| lot.contains(&ligne) && !joue)
        })
        .unwrap_or(false)
}

/// Oublie les marques de la zone (file vidée).
pub fn oublier_les_marques(db: &Arc<dyn DbBackend>, zone_id: i64) {
    let repo = SettingsRepo::with_backend(db.clone());
    if repo.get(&cle(zone_id)).ok().flatten().is_some()
        && let Err(e) = repo.delete(&cle(zone_id))
    {
        tracing::warn!(zone_id, error = %e, "radio_fin_de_file_marques_non_effacees");
    }
}

static FINS_VOULUES: LazyLock<Mutex<HashMap<i64, Vec<i64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// « Arrête-toi après ce qui est dans la file » : « Vider la suite », file
/// d'un réveil. Photographie la file telle qu'elle est maintenant.
pub fn noter_fin_voulue(db: &Arc<dyn DbBackend>, zone_id: i64) {
    let lignes = lignes_de_la_file(db, zone_id);
    if let Ok(mut m) = FINS_VOULUES.lock() {
        m.insert(zone_id, lignes);
    }
}

/// Consomme la photo de [`noter_fin_voulue`] : elle ne vaut que pour la
/// fin de file qui suit.
pub fn prendre_fin_voulue(zone_id: i64) -> Option<Vec<i64>> {
    FINS_VOULUES
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&zone_id))
}

/// La fin de file qui arrive est-elle une fin VOULUE ? Vrai si la file est
/// exactement celle photographiée par [`noter_fin_voulue`]. La consigne est
/// consommée dans tous les cas : elle ne vaut que pour cette fin-là.
pub fn fin_voulue(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    prendre_fin_voulue(zone_id).is_some_and(|lignes| lignes == lignes_de_la_file(db, zone_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_garde_les_plus_recentes_deja_jouees() {
        let file: Vec<i64> = (1..=10).collect();
        let marques: HashSet<i64> = [2, 3, 4, 5, 9].into_iter().collect();
        // Position 6 : la ligne 7 joue ; 2, 3, 4, 5 sont jouées, 9 à venir.
        assert_eq!(lignes_de_radio_a_purger(&file, &marques, 6, 2), vec![2, 3]);
        assert!(lignes_de_radio_a_purger(&file, &marques, 6, 50).is_empty());
    }

    #[test]
    fn la_cle_des_marques_est_reconnue() {
        assert!(cle_de_marques("zone_12_radio_fin_de_file"));
        assert!(cle_de_marques("zone_{id}_radio_fin_de_file"));
        assert!(!cle_de_marques("zone_12_radio_artiste"));
        assert!(!cle_de_marques("radio_fin_de_file"));
    }

    #[test]
    fn un_lot_qui_n_a_rien_joue_ne_relance_pas() {
        let zone = 2_694_001;
        noter_lot(zone, vec![10, 11]);
        assert!(lot_muet(zone, Some(11)));
        assert!(!lot_muet(zone, Some(99)), "fin hors du lot : pas concernée");
        noter_ligne_jouee(zone, 10);
        assert!(!lot_muet(zone, Some(11)));
    }
}
