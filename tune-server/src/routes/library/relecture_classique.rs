//! #8325 — le pilote de la relecture UNIQUE des balises classiques (œuvre,
//! mouvement, ensemble) des fichiers déjà importés : au premier démarrage
//! après la mise à jour, sans scan à lancer par l'utilisateur.
//!
//! - visible dans « Tâches de fond » sous [`TACHE`] ;
//! - pausable avec l'enrichissement des métadonnées (`tache_suspendable`) et
//!   cédant à la lecture (`attendre_son_tour`), comme les apports Qobuz ;
//! - basse priorité : des lots de [`TAILLE_DU_LOT`] pistes, une respiration
//!   entre deux, la lecture des fichiers hors de l'exécuteur ;
//! - une seule fois et reprenable : `tune_core::library::relecture_classique`.

use std::time::Duration;

use tune_core::library::relecture_classique as rc;
use tune_core::taches_de_fond::{Tache, attendre_son_tour};

use crate::state::AppState;

/// L'identifiant de la passe au registre des tâches de fond.
pub(crate) const TACHE: &str = "relecture_classique";
/// Pistes lues par lot.
const TAILLE_DU_LOT: i64 = 100;
/// La respiration entre deux lots.
const PAUSE_ENTRE_LOTS: Duration = Duration::from_millis(500);
/// Le délai après le démarrage : laisser passer la reprise des autres passes.
const DELAI_AU_DEMARRAGE: Duration = Duration::from_secs(5 * 60);

/// Au démarrage : la passe, une fois, si elle n'est pas déjà faite.
pub(crate) fn armer(state: &AppState) {
    if rc::est_faite(&state.backend) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DELAI_AU_DEMARRAGE).await;
        executer(&state).await;
    });
}

/// La passe entière. `false` : déjà en cours ou déjà faite.
pub(crate) async fn executer(state: &AppState) -> bool {
    if rc::est_faite(&state.backend) {
        return false;
    }
    let Some(_garde) = state.background_tasks.try_begin(
        TACHE,
        "Relecture des œuvres, mouvements et ensembles…",
        "maintenance",
    ) else {
        return false;
    };
    let total = {
        let b = state.backend.clone();
        tokio::task::spawn_blocking(move || rc::restantes(&b))
            .await
            .unwrap_or(0)
    };
    let mut faites = 0u64;
    state
        .background_tasks
        .update_progress(TACHE, 0, total, "Étiquettes");
    loop {
        attendre_son_tour(Tache::Enrichissement).await;
        let b = state.backend.clone();
        let lot = tokio::task::spawn_blocking(move || {
            rc::traiter_lot_suivant(&b, TAILLE_DU_LOT, rc::lire)
        })
        .await;
        match lot {
            Ok(Ok(lot)) if lot.fini => {
                tracing::info!(pistes = faites, "relecture_classique_terminee");
                break;
            }
            Ok(Ok(lot)) => {
                faites += lot.lues;
                state.background_tasks.update_progress(
                    TACHE,
                    faites,
                    total.max(faites),
                    "Étiquettes",
                );
            }
            Ok(Err(e)) => {
                // La base refuse : on s'arrête, le curseur reprendra au
                // prochain démarrage.
                tracing::warn!(error = %e, "relecture_classique_echec");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "relecture_classique_tache_perdue");
                break;
            }
        }
        tokio::time::sleep(PAUSE_ENTRE_LOTS).await;
    }
    true
}

#[cfg(test)]
#[path = "relecture_classique_8325_tests.rs"]
mod tests;
