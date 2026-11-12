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

/// Une relance demandée par le réglage « Balises du classique » attend la
/// passe en cours (ou part tout de suite) : les clés candidates à effacer.
static RELANCE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

fn prendre_la_relance() -> Option<Vec<String>> {
    RELANCE
        .lock()
        .map(|mut g| g.take())
        .unwrap_or_else(|e| e.into_inner().take())
}

fn relance_en_attente() -> bool {
    RELANCE
        .lock()
        .map(|g| g.is_some())
        .unwrap_or_else(|e| e.into_inner().is_some())
}

/// ESL63P — après une modification du réglage « Balises du classique » :
/// relire TOUTE la bibliothèque selon le nouveau réglage, par la même passe
/// de fond (pas de scan). Une passe déjà en cours repart de zéro à son
/// prochain lot ; sinon la passe part tout de suite.
pub(crate) fn relancer(state: &AppState, candidates: Vec<String>) {
    {
        let mut g = RELANCE.lock().unwrap_or_else(|e| e.into_inner());
        let toutes = g.get_or_insert_with(Vec::new);
        for c in candidates {
            if !toutes.contains(&c) {
                toutes.push(c);
            }
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        // Une passe qui finit pendant cet appel rend son verrou : on réessaie
        // tant qu'une relance reste en attente.
        while relance_en_attente() {
            if !executer(&state).await {
                if !relance_en_attente() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });
}

/// Applique une relance en attente : curseur à zéro, clés candidates
/// posées. `true` si une relance a été prise.
async fn appliquer_la_relance(state: &AppState) -> bool {
    let Some(candidates) = prendre_la_relance() else {
        return false;
    };
    let b = state.backend.clone();
    match tokio::task::spawn_blocking(move || rc::reinitialiser(&b, &candidates)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "relecture_classique_relance_echec");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "relecture_classique_relance_perdue");
            false
        }
    }
}

/// La passe entière. `false` : déjà en cours ou déjà faite.
pub(crate) async fn executer(state: &AppState) -> bool {
    if rc::est_faite(&state.backend) && !relance_en_attente() {
        return false;
    }
    let Some(_garde) = state.background_tasks.try_begin(
        TACHE,
        "Relecture des œuvres, mouvements et ensembles…",
        "maintenance",
    ) else {
        return false;
    };
    appliquer_la_relance(state).await;
    let mut total = {
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
        // Le réglage a changé pendant la passe : elle repart de zéro.
        if appliquer_la_relance(state).await {
            let b = state.backend.clone();
            total = tokio::task::spawn_blocking(move || rc::restantes(&b))
                .await
                .unwrap_or(0);
            faites = 0;
        }
        attendre_son_tour(Tache::Enrichissement).await;
        let b = state.backend.clone();
        let lot = tokio::task::spawn_blocking(move || {
            rc::traiter_lot_suivant(&b, TAILLE_DU_LOT, rc::lire)
        })
        .await;
        match lot {
            Ok(Ok(lot)) if lot.fini => {
                if relance_en_attente() {
                    continue;
                }
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
