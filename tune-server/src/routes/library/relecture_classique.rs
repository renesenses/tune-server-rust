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
use tune_core::metadata::balises_classique::{self, BalisesClassique};
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

/// Les relances dont la mise en base ([`rc::reinitialiser`]) a échoué : la
/// passe les retente à chaque tour, et les journalise tant qu'elles échouent.
static RELANCES_EN_ECHEC: std::sync::Mutex<Vec<BalisesClassique>> =
    std::sync::Mutex::new(Vec::new());

fn relances_en_echec() -> std::sync::MutexGuard<'static, Vec<BalisesClassique>> {
    RELANCES_EN_ECHEC.lock().unwrap_or_else(|e| e.into_inner())
}

/// Retente les relances en échec. `true` si toutes sont passées.
async fn retenter_les_relances(state: &AppState) -> bool {
    let en_attente: Vec<BalisesClassique> = std::mem::take(&mut *relances_en_echec());
    if en_attente.is_empty() {
        return true;
    }
    let b = state.backend.clone();
    let restees = tokio::task::spawn_blocking(move || {
        en_attente
            .into_iter()
            .filter(|ancien| match rc::reinitialiser(&b, ancien) {
                Ok(()) => false,
                Err(e) => {
                    tracing::warn!(error = %e, "relecture_classique_relance_echec_nouvel_essai");
                    true
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let ok = restees.is_empty();
    relances_en_echec().extend(restees);
    ok
}

/// ESL63P — après une modification du réglage « Balises du classique »
/// (`ancien` = le réglage d'avant) : la relance est mise EN BASE tout de
/// suite (curseur à zéro, passe rouverte, ancien réglage enregistré) — un
/// redémarrage la reprend par [`armer`]. Puis la passe part, ou une passe en
/// cours la prend à son lot suivant. Rend `false` si la mise en base a
/// échoué : la relance est alors gardée et retentée par la passe.
pub(crate) async fn relancer(state: &AppState, ancien: BalisesClassique) -> bool {
    let b = state.backend.clone();
    let a = ancien.clone();
    let persistee = matches!(
        tokio::task::spawn_blocking(move || rc::reinitialiser(&b, &a)).await,
        Ok(Ok(()))
    );
    if !persistee {
        tracing::warn!("relecture_classique_relance_non_persistee_nouvel_essai");
        relances_en_echec().push(ancien);
    }
    let state = state.clone();
    tokio::spawn(async move {
        // Une passe qui se termine pendant l'appel rend son verrou un instant
        // plus tard : on réessaie tant que la passe n'est pas close.
        for _ in 0..30 {
            if executer(&state).await {
                break;
            }
            let reste = !rc::est_faite(&state.backend) || !relances_en_echec().is_empty();
            if !reste {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    persistee
}

/// La passe entière. `false` : déjà en cours ou déjà faite.
pub(crate) async fn executer(state: &AppState) -> bool {
    if rc::est_faite(&state.backend) && relances_en_echec().is_empty() {
        return false;
    }
    let Some(_garde) = state.background_tasks.try_begin(
        TACHE,
        "Relecture des œuvres, mouvements et ensembles…",
        "maintenance",
    ) else {
        return false;
    };
    retenter_les_relances(state).await;
    let mut total = {
        let b = state.backend.clone();
        tokio::task::spawn_blocking(move || rc::restantes(&b))
            .await
            .unwrap_or(0)
    };
    let mut faites = 0u64;
    state
        .background_tasks
        .update_progress(TACHE, 0, total, crate::background_tasks::Phase::Tags);
    loop {
        // Une relance dont la mise en base avait échoué : nouvel essai.
        let en_echec = !relances_en_echec().is_empty();
        if en_echec && retenter_les_relances(state).await {
            let b = state.backend.clone();
            total = tokio::task::spawn_blocking(move || rc::restantes(&b))
                .await
                .unwrap_or(0);
            faites = 0;
        }
        attendre_son_tour(Tache::Enrichissement).await;
        let b = state.backend.clone();
        let lot = tokio::task::spawn_blocking(move || {
            let actuel = balises_classique::courante();
            rc::traiter_lot_suivant(&b, TAILLE_DU_LOT, &actuel, rc::lire)
        })
        .await;
        match lot {
            Ok(Ok(lot)) if lot.fini => {
                // Une relance arrivée entre le dernier lot et ici a rouvert
                // la passe, ou attend son nouvel essai : on continue.
                if !relances_en_echec().is_empty() || !rc::est_faite(&state.backend) {
                    tokio::time::sleep(PAUSE_ENTRE_LOTS).await;
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
                    crate::background_tasks::Phase::Tags,
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
