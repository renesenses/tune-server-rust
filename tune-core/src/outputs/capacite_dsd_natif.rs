//! Ce que chaque pilote ASIO a DÉCLARÉ du DSD natif (#5643, lot D).
//!
//! La capacité se sonde UNE fois par périphérique (bascule du pilote en DSD,
//! `kAsioCanSampleRate` pour chaque cadence, retour au PCM : voir
//! `asio_exclusive::sonder_cadences_dsd`), puis vit ici pour toute la durée
//! du processus. Le sondage n'a lieu que si le verrou de périphérique ASIO est
//! libre : jamais pendant la lecture d'un autre flux. S'il est occupé, rien
//! n'est écrit, et la piste part en DoP en le disant.
//!
//! Hors de tout `cfg` : l'API (`dsd_transport`) le lit sur toutes les
//! plateformes, et la règle se teste sous Linux. Sans pilote ASIO, la table
//! reste vide et tout se comporte comme avant #5643.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Ce qu'une sortie locale a déclaré, la dernière fois qu'on l'a regardée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapaciteConnue {
    /// La sortie ne passe pas par le bras ASIO exclusif (WASAPI, partagé,
    /// autre plateforme) : pas de DSD natif.
    SortieNonAsio,
    /// Pilote ASIO sondé : les cadences DSD qu'il accepte (vide = aucune,
    /// pilote PCM seulement).
    Asio { cadences: Vec<u32> },
}

fn table() -> &'static Mutex<HashMap<String, CapaciteConnue>> {
    static TABLE: OnceLock<Mutex<HashMap<String, CapaciteConnue>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// La capacité connue de la sortie `device_name` (le nom, sans le préfixe
/// `local:`).
#[must_use]
pub fn connue(device_name: &str) -> Option<CapaciteConnue> {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(device_name)
        .cloned()
}

/// Enregistre ce qu'on vient d'apprendre de `device_name`.
pub fn retenir(device_name: &str, capacite: CapaciteConnue) {
    table()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(device_name.to_string(), capacite);
}

/// Le pilote a déclaré `cadence` mais l'ouverture native l'a refusée : on la
/// retire, pour que la piste suivante parte en DoP au lieu d'échouer encore.
pub fn oublier_cadence(device_name: &str, cadence: u32) {
    let mut t = table().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(CapaciteConnue::Asio { cadences }) = t.get_mut(device_name) {
        cadences.retain(|c| *c != cadence);
    }
}

/// Ce que l'API publie : « natif » si la sortie `output_device_id`
/// (`local:<nom>`) est connue comme ASIO avec au moins une cadence DSD
/// déclarée. Ne sonde jamais : une route de lecture ne touche pas au pilote.
#[must_use]
pub fn natif_annonce(output_device_id: Option<&str>) -> bool {
    let Some(nom) = output_device_id.and_then(|id| id.strip_prefix("local:")) else {
        return false;
    };
    matches!(connue(nom), Some(CapaciteConnue::Asio { cadences }) if !cadences.is_empty())
}
