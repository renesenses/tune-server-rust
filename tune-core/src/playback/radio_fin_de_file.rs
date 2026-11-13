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
//!   toute façon arrêtée.

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

/// Les pistes de bibliothèque déjà dans la file : la radio ne les repropose
/// pas (pas de doublon avec ce qui vient d'être joué ou attend).
pub fn pistes_locales_de_la_file(db: &Arc<dyn DbBackend>, zone_id: i64) -> HashSet<i64> {
    PlayQueueRepo::with_backend(db.clone())
        .get_ordered(zone_id)
        .map(|rows| rows.into_iter().filter_map(|r| r.track_id).collect())
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

/// Marque comme « ajoutées par la radio » les lignes de la file qui
/// n'étaient pas dans `avant`. Les marques des lignes sorties de la file
/// sont oubliées au passage. Rend le nombre de lignes marquées.
pub fn marquer_les_ajouts(db: &Arc<dyn DbBackend>, zone_id: i64, avant: &[i64]) -> usize {
    let maintenant = lignes_de_la_file(db, zone_id);
    let avant: HashSet<i64> = avant.iter().copied().collect();
    let nouvelles: Vec<i64> = maintenant
        .iter()
        .copied()
        .filter(|id| !avant.contains(id))
        .collect();
    if nouvelles.is_empty() {
        return 0;
    }
    let deja = lignes_ajoutees_par_la_radio(db, zone_id);
    let mut gardees: Vec<i64> = maintenant
        .iter()
        .copied()
        .filter(|id| deja.contains(id) || nouvelles.contains(id))
        .collect();
    if gardees.len() > MARQUES_MAX {
        gardees.drain(..gardees.len() - MARQUES_MAX);
    }
    let valeur = serde_json::to_string(&gardees).unwrap_or_else(|_| "[]".into());
    if let Err(e) = SettingsRepo::with_backend(db.clone()).set(&cle(zone_id), &valeur) {
        tracing::warn!(zone_id, error = %e, "radio_fin_de_file_marques_non_ecrites");
    }
    nouvelles.len()
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

/// La fin de file qui arrive est-elle une fin VOULUE ? Vrai si la file est
/// exactement celle photographiée par [`noter_fin_voulue`]. La consigne est
/// consommée dans tous les cas : elle ne vaut que pour cette fin-là.
pub fn fin_voulue(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    let photo = FINS_VOULUES
        .lock()
        .ok()
        .and_then(|mut m| m.remove(&zone_id));
    match photo {
        Some(lignes) => lignes == lignes_de_la_file(db, zone_id),
        None => false,
    }
}
