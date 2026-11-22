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
//! La règle, ici : quand le traitement dont la jauge dépend est suspendu ET
//! que la bibliothèque n'a pas bougé depuis le relevé (même
//! [`tune_core::db::library_revision`]), la jauge rend son **dernier
//! relevé**, sans comptage. Une écriture pendant la pause (scan, suppression,
//! une autre passe qui travaille, la pause elle-même) change la révision : le
//! relevé suivant recompte, une fois. Hors pause, chaque appel recompte,
//! exactement comme avant. Sans révision lisible, rien n'est resservi : plus
//! lent, jamais faux.
//!
//! Le comptage tourne sur le pool bloquant (jamais sur un fil de
//! l'exécuteur), dans une tâche détachée : un client qui abandonne ne perd
//! pas le relevé (#8805). Un seul comptage à la fois par relevé ; un appel qui
//! attendait reprend le relevé dont le comptage a COMMENCÉ après son arrivée,
//! au lieu d'en relancer un : hors pause, N sondes simultanées coûtent au plus
//! deux comptages, pas N à la file.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// La clé d'un relevé : la révision de la bibliothèque lue juste AVANT le
/// comptage (`None` : illisible, jamais resservi).
pub(crate) type Revision = Option<(i64, i64)>;

struct Releve<T> {
    valeur: T,
    /// Début du comptage : un appel arrivé avant peut le reprendre.
    debut: Instant,
    revision: Revision,
}

/// Un relevé figé pendant la pause de ce qu'il mesure.
pub(crate) struct ReleveSuspendu<T> {
    dernier: tokio::sync::Mutex<Option<Releve<T>>>,
}

impl<T> Default for ReleveSuspendu<T> {
    fn default() -> Self {
        Self {
            dernier: tokio::sync::Mutex::new(None),
        }
    }
}

/// Le dernier relevé est-il bon à resservir ? Pure, pour l'éprouver sans
/// horloge ni base : seulement en pause, seulement si la bibliothèque porte
/// encore la révision sous laquelle il a été compté, et — si une durée est
/// donnée — tant qu'il est plus jeune qu'elle.
pub(crate) fn resservir(
    suspendu: bool,
    revision_du_releve: Revision,
    revision_actuelle: Revision,
    age: Duration,
    duree_max: Option<Duration>,
) -> bool {
    suspendu
        && revision_actuelle.is_some()
        && revision_du_releve == revision_actuelle
        && duree_max.is_none_or(|d| age < d)
}

/// Un appel arrivé à `arrivee` peut-il reprendre le relevé dont le comptage
/// a commencé à `debut` ? Oui s'il a commencé après : il voit au moins tout
/// ce que l'appel aurait vu.
pub(crate) fn compte_apres_l_arrivee(debut: Instant, arrivee: Instant) -> bool {
    debut >= arrivee
}

/// La révision de la bibliothèque de `backend`, à lire sur le pool bloquant.
pub(crate) fn revision_de(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
) -> impl FnOnce() -> Revision + Send + 'static {
    let b = backend.clone();
    move || tune_core::db::library_revision::lire(b.as_ref())
}

/// Ce qui empêche un relevé.
pub(crate) enum Manque<E> {
    /// Le comptage a rendu une erreur : jamais gardée.
    Erreur(E),
    /// La tâche de comptage est perdue (panique) et aucun relevé n'existe.
    Perdu,
}

impl<T: Clone + Send + Sync + 'static> ReleveSuspendu<T> {
    /// Le relevé : le dernier connu si [`resservir`] le permet, sinon
    /// recompté par `compter` sur le pool bloquant. `None` seulement si la
    /// tâche de comptage a été perdue (panique) et qu'aucun relevé n'existe.
    pub(crate) async fn lire<R, F>(
        self: &Arc<Self>,
        route: &'static str,
        suspendu: bool,
        revision: R,
        compter: F,
    ) -> Option<T>
    where
        R: FnOnce() -> Revision + Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.lire_selon(route, suspendu, None, revision, move || {
            Ok::<T, std::convert::Infallible>(compter())
        })
        .await
        .ok()
    }

    /// [`Self::lire`], avec une durée de vie et un comptage faillible.
    pub(crate) async fn lire_selon<E, R, F>(
        self: &Arc<Self>,
        route: &'static str,
        suspendu: bool,
        duree_max: Option<Duration>,
        revision: R,
        compter: F,
    ) -> Result<T, Manque<E>>
    where
        E: Send + 'static,
        R: FnOnce() -> Revision + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        let arrivee = Instant::now();
        let releve = Arc::clone(self);
        // Tâche à part : le comptage n'est pas annulé avec la requête, son
        // relevé arrive quand même pour l'appel suivant (#8805).
        tokio::spawn(async move {
            releve
                .lire_sans_annulation(route, arrivee, suspendu, duree_max, revision, compter)
                .await
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(route, error = %e, "releve_suspendu_tache_perdue_9116");
            Err(Manque::Perdu)
        })
    }

    async fn lire_sans_annulation<E, R, F>(
        &self,
        route: &'static str,
        arrivee: Instant,
        suspendu: bool,
        duree_max: Option<Duration>,
        revision: R,
        compter: F,
    ) -> Result<T, Manque<E>>
    where
        E: Send + 'static,
        R: FnOnce() -> Revision + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        // Tenu pendant le comptage : les appels concurrents l'attendent en
        // `await` au lieu d'en lancer un second.
        let mut dernier = self.dernier.lock().await;
        if let Some(r) = dernier.as_ref()
            && compte_apres_l_arrivee(r.debut, arrivee)
        {
            return Ok(r.valeur.clone());
        }
        // En pause seulement : une lecture de la révision (une ligne par clé
        // primaire), hors de l'exécuteur, décide si le relevé tient encore.
        let mut revision = Some(revision);
        let revision_lue = if suspendu
            && dernier.is_some()
            && let Some(lire_revision) = revision.take()
        {
            match tokio::task::spawn_blocking(lire_revision).await {
                Ok(rev) => {
                    if let Some(r) = dernier.as_ref()
                        && resservir(true, r.revision, rev, r.debut.elapsed(), duree_max)
                    {
                        return Ok(r.valeur.clone());
                    }
                    Some(rev)
                }
                Err(e) => {
                    tracing::error!(route, error = %e, "releve_suspendu_revision_perdue_9116");
                    Some(None)
                }
            }
        } else {
            None
        };
        let debut = Instant::now();
        let compte = tokio::task::spawn_blocking(move || {
            // Lue AVANT le comptage : une écriture pendant le comptage laisse
            // au relevé une révision déjà dépassée, le suivant recomptera.
            let rev = match (revision_lue, revision) {
                (Some(rev), _) => rev,
                (None, Some(lire_revision)) => lire_revision(),
                (None, None) => None,
            };
            (rev, compter())
        })
        .await;
        match compte {
            Ok((revision, Ok(valeur))) => {
                *dernier = Some(Releve {
                    valeur: valeur.clone(),
                    debut,
                    revision,
                });
                Ok(valeur)
            }
            Ok((_, Err(e))) => Err(Manque::Erreur(e)),
            Err(e) => {
                tracing::error!(route, error = %e, "releve_suspendu_tache_perdue_9116");
                dernier
                    .as_ref()
                    .map(|r| r.valeur.clone())
                    .ok_or(Manque::Perdu)
            }
        }
    }
}

/// Durée de vie d'un relevé GÉNÉRAL de la bibliothèque (`/library/stats`,
/// `/library/stats/completeness`) quand tout est suspendu : ces écrans
/// portent aussi l'écoute, les zones et des reports datés, que la révision ne
/// suit pas. Une minute au plus, puis un recomptage.
pub(crate) const DUREE_RELEVE_GENERAL: Duration = Duration::from_secs(60);

/// Tout est-il au repos, scan compris ? Pure : la décision de
/// [`bibliotheque_au_repos`], éprouvée sur toutes les combinaisons.
pub(crate) fn au_repos_selon(tout_suspendu: bool, scan: bool) -> bool {
    tout_suspendu && !scan
}

/// Tout est-il au repos, scan compris ? C'est la condition des relevés
/// généraux : un scan n'est pas suspendable et ajoute des pistes.
pub(crate) fn bibliotheque_au_repos() -> bool {
    au_repos_selon(
        tune_core::taches_de_fond::tout_est_suspendu(),
        tune_core::scanner::activite::scan_bibliotheque_en_cours(),
    )
}

/// Un corps de route général (`/library/stats`, `/library/stats/completeness`),
/// resservi quand la bibliothèque est au repos ([`bibliotheque_au_repos`]),
/// sous la même révision et depuis moins de [`DUREE_RELEVE_GENERAL`] ;
/// recompté sinon. Une erreur n'est jamais gardée et remonte telle quelle,
/// statut compris.
pub(crate) async fn corps_general<F>(
    releve: &Arc<ReleveSuspendu<serde_json::Value>>,
    route: &'static str,
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    lire: F,
) -> Result<serde_json::Value, crate::error::AppError>
where
    F: FnOnce() -> Result<serde_json::Value, crate::error::AppError> + Send + 'static,
{
    corps_general_selon(
        releve,
        route,
        bibliotheque_au_repos(),
        revision_de(backend),
        lire,
    )
    .await
}

/// [`corps_general`], l'état « au repos » et la révision fournis par
/// l'appelant. Les témoins passent un état fixe : le vrai dépend de compteurs
/// du processus (pauses, scans) que d'autres tests, en parallèle, font bouger.
pub(crate) async fn corps_general_selon<R, F>(
    releve: &Arc<ReleveSuspendu<serde_json::Value>>,
    route: &'static str,
    au_repos: bool,
    revision: R,
    lire: F,
) -> Result<serde_json::Value, crate::error::AppError>
where
    R: FnOnce() -> Revision + Send + 'static,
    F: FnOnce() -> Result<serde_json::Value, crate::error::AppError> + Send + 'static,
{
    match releve
        .lire_selon(route, au_repos, Some(DUREE_RELEVE_GENERAL), revision, lire)
        .await
    {
        Ok(v) => Ok(v),
        Err(Manque::Erreur(e)) => Err(e),
        Err(Manque::Perdu) => Err(crate::error::AppError::internal(format!(
            "{route} : lecture interrompue"
        ))),
    }
}

/// Les relevés tenus par l'état du serveur — un jeu par état, pour qu'un
/// témoin ne pollue pas le suivant.
#[derive(Default)]
pub(crate) struct RelevesSuspendus {
    /// `/system/replaygain/progress` : candidats, reportées, bibliothèque.
    pub(crate) rg_candidats: Arc<ReleveSuspendu<i64>>,
    pub(crate) rg_reportees: Arc<ReleveSuspendu<i64>>,
    pub(crate) rg_bibliotheque: Arc<
        ReleveSuspendu<Option<tune_core::audio::replaygain::bibliotheque::BibliothequeReplayGain>>,
    >,
    /// `/system/dynamic-range/progress` : candidats.
    pub(crate) dr_candidats: Arc<ReleveSuspendu<i64>>,
    /// `/library/search/acoustic/status` : les cinq compteurs de la jauge.
    #[cfg(feature = "audio-embedding")]
    pub(crate) acoustique: Arc<ReleveSuspendu<[i64; 5]>>,
    /// `/library/stats/completeness` et `/library/stats` : le corps entier.
    pub(crate) completude: Arc<ReleveSuspendu<serde_json::Value>>,
    pub(crate) stats_bibliotheque: Arc<ReleveSuspendu<serde_json::Value>>,
}

#[cfg(test)]
#[path = "avancement_suspendu_9116_tests.rs"]
mod tests;
