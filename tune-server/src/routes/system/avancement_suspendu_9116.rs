//! #9116 (fil 2935) — « Suspendre les traitements » suspend aussi les
//! compteurs d'avancement des écrans d'état.
//!
//! Le rapport : 544 147 pistes sur SQLite, TOUTES les passes en pause, et
//! l'écran « État du serveur » qui sonde toutes les 5 s une quinzaine de
//! routes. Plusieurs recomptaient l'avancement de chaque passe sur toute la
//! bibliothèque — `COUNT(*)` à trois ou cinq `NOT EXISTS` sur
//! `track_metadata`, 1,5 à 2 s chacun — alors qu'aucune passe ne pouvait plus
//! faire bouger ces chiffres. Les connexions de lecture étaient prises, et les
//! requêtes triviales (file d'une zone, profil) attendaient jusqu'à 3,6 s.
//!
//! La règle, ici : quand le traitement dont la jauge dépend est suspendu, la
//! jauge rend son **dernier relevé**, sans requête. Le tout premier relevé
//! (serveur qui démarre déjà en pause) est pris une fois, puis figé. Dès que
//! le traitement reprend, chaque appel recompte, exactement comme avant.
//!
//! Le comptage tourne sur le pool bloquant (jamais sur un fil de
//! l'exécuteur), et un seul à la fois par relevé : les sondes concurrentes
//! attendent le comptage en vol en `await`, sans en lancer un second.

use std::time::{Duration, Instant};

/// Un relevé figé pendant la pause de ce qu'il mesure.
pub(crate) struct ReleveSuspendu<T> {
    dernier: tokio::sync::Mutex<Option<(T, Instant)>>,
}

impl<T> Default for ReleveSuspendu<T> {
    fn default() -> Self {
        Self {
            dernier: tokio::sync::Mutex::new(None),
        }
    }
}

/// Le dernier relevé est-il bon à resservir ? Pure, pour l'éprouver sans
/// horloge : seulement en pause, et — si une durée est donnée — tant qu'il
/// est plus jeune qu'elle.
pub(crate) fn resservir(suspendu: bool, age: Duration, duree_max: Option<Duration>) -> bool {
    suspendu && duree_max.is_none_or(|d| age < d)
}

impl<T: Clone + Send + 'static> ReleveSuspendu<T> {
    /// Le relevé : le dernier connu si [`resservir`] le permet, sinon
    /// recompté par `compter` sur le pool bloquant. `None` seulement si la
    /// tâche de comptage a été perdue (panique) et qu'aucun relevé n'existe.
    pub(crate) async fn lire<F>(
        &self,
        route: &'static str,
        suspendu: bool,
        duree_max: Option<Duration>,
        compter: F,
    ) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        // Tenu pendant le comptage : les sondes concurrentes l'attendent.
        let mut dernier = self.dernier.lock().await;
        if let Some((v, pris_a)) = dernier.as_ref()
            && resservir(suspendu, pris_a.elapsed(), duree_max)
        {
            return Some(v.clone());
        }
        match tokio::task::spawn_blocking(compter).await {
            Ok(v) => {
                *dernier = Some((v.clone(), Instant::now()));
                Some(v)
            }
            Err(e) => {
                tracing::error!(route, error = %e, "releve_suspendu_tache_perdue_9116");
                dernier.as_ref().map(|(v, _)| v.clone())
            }
        }
    }
}

/// Durée de vie d'un relevé GÉNÉRAL de la bibliothèque (`/library/stats`,
/// `/library/stats/completeness`) quand tout est suspendu : ces écrans
/// portent aussi l'écoute et les zones, qui bougent pendant la pause. Une
/// minute, puis un recomptage.
pub(crate) const DUREE_RELEVE_GENERAL: Duration = Duration::from_secs(60);

/// Tout est-il au repos, scan compris ? C'est la condition des relevés
/// généraux : un scan n'est pas suspendable et ajoute des pistes.
pub(crate) fn bibliotheque_au_repos() -> bool {
    tune_core::taches_de_fond::tout_est_suspendu()
        && !tune_core::scanner::activite::scan_bibliotheque_en_cours()
}

/// Un corps de route général (`/library/stats`, `/library/stats/completeness`),
/// resservi [`DUREE_RELEVE_GENERAL`] quand la bibliothèque est au repos
/// ([`bibliotheque_au_repos`]), recompté à chaque appel sinon. Une erreur
/// n'est jamais gardée et remonte telle quelle, statut compris.
pub(crate) async fn corps_general<F>(
    releve: &ReleveSuspendu<serde_json::Value>,
    route: &'static str,
    lire: F,
) -> Result<serde_json::Value, crate::error::AppError>
where
    F: FnOnce() -> Result<serde_json::Value, crate::error::AppError> + Send + 'static,
{
    let mut dernier = releve.dernier.lock().await;
    if let Some((v, pris_a)) = dernier.as_ref()
        && resservir(
            bibliotheque_au_repos(),
            pris_a.elapsed(),
            Some(DUREE_RELEVE_GENERAL),
        )
    {
        return Ok(v.clone());
    }
    match tokio::task::spawn_blocking(lire).await {
        Ok(Ok(v)) => {
            *dernier = Some((v.clone(), Instant::now()));
            Ok(v)
        }
        Ok(Err(e)) => Err(e),
        Err(e) => {
            tracing::error!(route, error = %e, "releve_suspendu_tache_perdue_9116");
            Err(crate::error::AppError::internal(format!(
                "{route} : lecture interrompue"
            )))
        }
    }
}

/// Les relevés tenus par l'état du serveur — un jeu par état, pour qu'un
/// témoin ne pollue pas le suivant.
#[derive(Default)]
pub(crate) struct RelevesSuspendus {
    /// `/system/replaygain/progress` : candidats, reportées, bibliothèque.
    pub(crate) rg_candidats: ReleveSuspendu<i64>,
    pub(crate) rg_reportees: ReleveSuspendu<i64>,
    pub(crate) rg_bibliotheque:
        ReleveSuspendu<Option<tune_core::audio::replaygain::bibliotheque::BibliothequeReplayGain>>,
    /// `/system/dynamic-range/progress` : candidats.
    pub(crate) dr_candidats: ReleveSuspendu<i64>,
    /// `/library/search/acoustic/status` : les cinq compteurs de la jauge.
    #[cfg(feature = "audio-embedding")]
    pub(crate) acoustique: ReleveSuspendu<[i64; 5]>,
    /// `/library/stats/completeness` et `/library/stats` : le corps entier.
    pub(crate) completude: ReleveSuspendu<serde_json::Value>,
    pub(crate) stats_bibliotheque: ReleveSuspendu<serde_json::Value>,
}

#[cfg(test)]
#[path = "avancement_suspendu_9116_tests.rs"]
mod tests;
