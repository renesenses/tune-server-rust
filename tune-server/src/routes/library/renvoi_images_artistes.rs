//! Le pilote du renvoi UNIQUE des images d'artistes perdues par le site
//! (rc12, 13/11/2026) : `tune_core::cloud::renvoi_images_artistes`.
//!
//! - une seule fois (marqueur constant en réglage), et seulement si
//!   l'utilisateur a consenti au partage communautaire ;
//! - visible dans « Ce qui tourne » et les tâches de fond sous [`TACHE`] ;
//! - pausable avec les images d'artistes (`tache_suspendable`) et cédant à la
//!   lecture (`attendre_son_tour`) avant chaque envoi ;
//! - un envoi toutes les 2 s au plus ; un site injoignable ou en 503 remet
//!   la passe à plus tard, sans marqueur.

use std::time::Duration;

use tune_core::cloud::renvoi_images_artistes as renvoi;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::taches_de_fond::{Tache, attendre_son_tour};

use crate::background_tasks::Phase;
use crate::state::AppState;

/// L'identifiant de la passe au registre des tâches de fond.
pub(crate) const TACHE: &str = "renvoi_images_artistes";
/// Le délai après le démarrage : laisser passer la reprise des autres passes.
const DELAI_AU_DEMARRAGE: Duration = Duration::from_secs(10 * 60);

/// La version qui fait le renvoi, gardée dans le marqueur pour le diagnostic.
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Au démarrage : la passe, une seule fois, si elle n'est pas faite.
pub(crate) fn armer(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if renvoi::deja_fait(&settings) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DELAI_AU_DEMARRAGE).await;
        executer(&state, None).await;
    });
}

/// La passe entière. `reglages` : ceux de production si `None` (les témoins
/// passent un faux site). Rend `None` quand elle n'a pas tourné (déjà en
/// cours, déjà faite, pas de consentement).
pub(crate) async fn executer(
    state: &AppState,
    reglages: Option<renvoi::Reglages>,
) -> Option<renvoi::Bilan> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if renvoi::deja_fait(&settings) || !tune_core::cloud::consent::contribution_autorisee(&settings)
    {
        return None;
    }
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    if instance_id.is_empty() {
        return None;
    }
    let reglages = reglages.unwrap_or_else(|| {
        renvoi::Reglages::production(
            instance_id,
            version().to_string(),
            super::artwork_cache_dir(),
        )
    });
    let _garde =
        state
            .background_tasks
            .try_begin(TACHE, "Partage des images d'artistes…", "enrichment")?;
    let taches = state.background_tasks.clone();
    let consent = SettingsRepo::with_backend(state.backend.clone());
    let bilan = renvoi::executer(
        state.backend.clone(),
        &reglages,
        || tune_core::cloud::consent::contribution_autorisee(&consent),
        || attendre_son_tour(Tache::ImagesArtistes),
        |fait, total| taches.update_progress(TACHE, fait, total, Phase::Images),
    )
    .await;
    tracing::info!(
        examines = bilan.examines,
        renvoyees = bilan.renvoyees,
        terminee = bilan.terminee,
        "renvoi_images_artistes_bilan"
    );
    Some(bilan)
}

#[cfg(test)]
#[path = "renvoi_images_artistes_tests.rs"]
mod tests;
