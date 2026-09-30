//! #5104 (fil 1954) — dire au journal pourquoi une piste n'a eu AUCUN niveau.
//!
//! Le spectre reste à plat après un enchaînement, deux fois en quatre jours
//! chez Didier. Le chemin est `advance_queue_metadata` → `bump_levels_gen` →
//! `fichier_a_mesurer_apres_avance` → `spawn_local_file_levels_decode` →
//! `spawn_paced_levels_forwarder`, et chacune de ses sorties était muette :
//! le forwarder rendait la main sans rien écrire (`play_seq` ou génération
//! changés, zone jamais passée en lecture, lecture remplacée pendant l'attente
//! de la sortie), et l'échec du décodage-pour-niveaux ne sortait qu'en DEBUG.
//! Un journal de testeur ne départageait donc rien.
//!
//! Deux lignes INFO, et rien d'autre :
//!
//! - `gapless_levels_after_advance` : à l'enchaînement, la mesure est-elle
//!   lancée, et sinon pourquoi ;
//! - `levels_forwarder_stopped_unpublished` : un forwarder meurt sans avoir
//!   publié une seule trame, avec son motif et ce qu'il a reçu.
//!
//! **Débit limité** : une ligne de chaque sorte par piste au plus, la piste
//! étant désignée par `(zone, play_seq, génération de niveaux)`. La table ne
//! garde que la dernière piste dite par zone : elle ne grossit pas avec le
//! temps.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use tracing::info;

/// Pourquoi un forwarder de niveaux s'est arrêté.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ArretDuForwarder {
    /// La file des fenêtres s'est fermée : décodage fini, en échec, ou puits
    /// arrêté.
    FluxClos,
    /// Une autre lecture a commencé sur la zone.
    PlaySeqChange,
    /// Une avance gapless a invalidé les forwarders de la piste.
    GenerationChangee,
    /// La zone s'est arrêtée après avoir joué.
    ZoneArretee,
    /// La zone n'est pas passée en lecture dans le délai de démarrage.
    DelaiDeDemarrage,
    /// Piste remplacée ou lecture arrêtée pendant l'attente de l'horloge de
    /// la sortie locale.
    RemplaceePendantLAttenteDeSortie,
}

impl ArretDuForwarder {
    pub(super) fn motif(self) -> &'static str {
        match self {
            Self::FluxClos => "flux_clos",
            Self::PlaySeqChange => "play_seq_change",
            Self::GenerationChangee => "generation_changee",
            Self::ZoneArretee => "zone_arretee",
            Self::DelaiDeDemarrage => "delai_de_demarrage",
            Self::RemplaceePendantLAttenteDeSortie => "remplacee_pendant_l_attente_de_sortie",
        }
    }
}

const LIGNE_ARRET: &str = "levels_forwarder_stopped_unpublished";
const LIGNE_AVANCE: &str = "gapless_levels_after_advance";

/// Dernière piste dite `(play_seq, génération)`, par zone et par sorte de ligne.
type DernierePisteDite = HashMap<(i64, &'static str), (u64, u64)>;

static DEJA_DIT: LazyLock<Mutex<DernierePisteDite>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Vrai la première fois que `ligne` est demandée pour cette piste.
fn premiere_fois(ligne: &'static str, zone_id: i64, play_seq: u64, generation: u64) -> bool {
    let mut table = DEJA_DIT.lock().unwrap_or_else(|e| e.into_inner());
    table.insert((zone_id, ligne), (play_seq, generation)) != Some((play_seq, generation))
}

/// Le forwarder s'arrête. Rien n'est écrit s'il a publié au moins une trame :
/// un arrêt après publication est la vie normale d'une piste.
pub(super) fn arret_du_forwarder(
    zone_id: i64,
    play_seq: u64,
    generation: u64,
    arret: ArretDuForwarder,
    trames_publiees: u64,
    fenetres_recues: u64,
    fenetres_sautees: u64,
) {
    if trames_publiees > 0 || !premiere_fois(LIGNE_ARRET, zone_id, play_seq, generation) {
        return;
    }
    info!(
        zone_id,
        play_seq,
        generation,
        motif = arret.motif(),
        fenetres_recues,
        fenetres_sautees,
        "levels_forwarder_stopped_unpublished"
    );
}

/// À l'enchaînement : la mesure de la piste devenue courante est-elle lancée ?
/// `decision` nomme la branche prise (`decodage_du_fichier`,
/// `sonde_du_service`, ou `non_lance_…` avec la raison).
pub(super) fn niveaux_apres_avance(
    zone_id: i64,
    play_seq: u64,
    generation: u64,
    track_id: Option<i64>,
    format: Option<&str>,
    decision: &'static str,
) {
    if !premiere_fois(LIGNE_AVANCE, zone_id, play_seq, generation) {
        return;
    }
    info!(
        zone_id,
        play_seq, generation, track_id, format, decision, "gapless_levels_after_advance"
    );
}

#[cfg(test)]
mod tests;
