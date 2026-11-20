//! #8805 (fil 2841) — les comptages des routes sondées en boucle, HORS des
//! fils de l'exécuteur, et resservis tant qu'ils sont lents.
//!
//! Le rapport : un conteneur Docker à 4 processeurs, 120 448 pistes, un scan
//! complet qui sature le disque (iowait 90 à 95 %). `GET /system/stats` (×2) et
//! `GET /system/health/monitor` lançaient chacun un `SELECT COUNT(*) FROM
//! tracks` DIRECTEMENT dans le gestionnaire async ; sous cette attente disque,
//! chaque comptage a tenu un fil `tokio-rt-worker` plus de 100 s. Trois fils
//! sur quatre étaient pris, le quatrième suivait, et le serveur entier ne
//! répondait plus. `/system/health`, que la barre latérale et la boucle de
//! reconnexion (700 ms) sondent, comptait de la même façon.
//!
//! Deux remèdes, ici :
//!
//! 1. **`spawn_blocking`** — même famille que #5438 (`hors_executeur` des
//!    routes de bibliothèque) : une lecture longue ne retient plus qu'un fil du
//!    pool bloquant, jamais un fil de l'exécuteur.
//! 2. **Un seul comptage en vol par route, resservi s'il a été lent.** Le pool
//!    bloquant ne suffit pas seul : quatre sondes par seconde qui lancent
//!    chacune un comptage de 100 s, c'est des centaines de lectures empilées
//!    sur un disque déjà saturé, et les connexions de lecture prises (13
//!    lectures de `settings`, `profiles`, `zones` attendaient une connexion
//!    depuis 47 à 74 s dans le rapport). Les appels concurrents attendent donc
//!    le comptage en vol — en `await`, sans tenir de fil — et un comptage qui
//!    a pris plus de [`SEUIL_DE_LENTEUR`] est resservi [`DUREE_DE_RESERVE`].
//!
//! Pourquoi « lent » plutôt que « pendant un scan » : c'est la lenteur qui
//! fait le dégât, quelle qu'en soit la cause (scan, sauvegarde, disque qui
//! meurt, NAS voisin). Et un serveur sain, où le comptage prend quelques
//! millisecondes, recompte À CHAQUE appel exactement comme avant : aucun
//! compteur figé après un ajout, aucun test qui lirait un vieux chiffre.
//!
//! Le comptage tourne dans une tâche à part : un client qui abandonne sa
//! requête (délai dépassé côté navigateur) ne perd pas le travail, le relevé
//! arrive quand même dans le cache pour l'appel suivant.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::routes::library::stats::VentilationParSource;
use crate::state::AppState;

/// Au-delà, le comptage est « lent » et son relevé est resservi. Sur un
/// serveur sain, `COUNT(*)` sur 120 000 pistes prend quelques millisecondes.
pub(crate) const SEUIL_DE_LENTEUR: Duration = Duration::from_secs(1);

/// Combien de temps un relevé lent est resservi avant de recompter.
pub(crate) const DUREE_DE_RESERVE: Duration = Duration::from_secs(30);

/// Un relevé lent reste-t-il bon à resservir ? Pure, pour l'éprouver sans
/// horloge.
pub(crate) fn encore_valable(duree_du_comptage: Duration, age: Duration) -> bool {
    duree_du_comptage >= SEUIL_DE_LENTEUR && age < DUREE_DE_RESERVE
}

struct Releve<T> {
    valeur: T,
    pris_a: Instant,
    duree: Duration,
}

/// Un comptage à la fois, hors exécuteur, resservi s'il a été lent.
pub(crate) struct CacheDeComptage<T> {
    dernier: tokio::sync::Mutex<Option<Releve<T>>>,
}

impl<T> Default for CacheDeComptage<T> {
    fn default() -> Self {
        Self {
            dernier: tokio::sync::Mutex::new(None),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> CacheDeComptage<T> {
    /// Le relevé : resservi s'il est lent et récent, sinon recompté par
    /// `compter` sur le pool bloquant. `Err` seulement si `compter` a paniqué.
    pub(crate) async fn lire<F>(
        self: &Arc<Self>,
        route: &'static str,
        compter: F,
    ) -> Result<T, String>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        let cache = Arc::clone(self);
        // Tâche à part : le comptage n'est pas annulé avec la requête.
        tokio::spawn(async move { cache.lire_sans_annulation(route, compter).await })
            .await
            .map_err(|e| format!("{route} : comptage interrompu ({e})"))?
    }

    async fn lire_sans_annulation<F>(&self, route: &'static str, compter: F) -> Result<T, String>
    where
        F: FnOnce() -> T + Send + 'static,
    {
        // Tenu pendant le comptage : les appels concurrents l'attendent en
        // `await` au lieu d'en lancer un second.
        let mut dernier = self.dernier.lock().await;
        if let Some(r) = dernier.as_ref()
            && encore_valable(r.duree, r.pris_a.elapsed())
        {
            return Ok(r.valeur.clone());
        }
        let debut = Instant::now();
        match tokio::task::spawn_blocking(compter).await {
            Ok(valeur) => {
                let duree = debut.elapsed();
                if duree >= SEUIL_DE_LENTEUR {
                    tracing::warn!(
                        route,
                        ms = duree.as_millis() as u64,
                        "comptage_lent_resservi_8805"
                    );
                }
                *dernier = Some(Releve {
                    valeur: valeur.clone(),
                    pris_a: Instant::now(),
                    duree,
                });
                Ok(valeur)
            }
            Err(e) => {
                tracing::error!(route, error = %e, "comptage_tache_bloquante_perdue_8805");
                Err(format!("{route} : comptage interrompu ({e})"))
            }
        }
    }
}

/// Ce que comptent `/system/health` et `/system/health/monitor`.
#[derive(Clone)]
pub(crate) struct ComptesSante {
    pub tracks: Result<i64, String>,
    pub albums: Result<i64, String>,
}

impl ComptesSante {
    pub(crate) fn compter(state: &AppState) -> Self {
        Self {
            tracks: TrackRepo::with_backend(state.backend.clone())
                .count()
                .map_err(|e| e.to_string()),
            albums: AlbumRepo::with_backend(state.backend.clone())
                .count()
                .map_err(|e| e.to_string()),
        }
    }

    fn en_echec(erreur: String) -> Self {
        Self {
            tracks: Err(erreur.clone()),
            albums: Err(erreur),
        }
    }
}

/// Ce que compte `/system/stats`.
#[derive(Clone)]
pub(crate) struct ComptesStats {
    pub artists: i64,
    pub albums: i64,
    pub tracks: i64,
    pub listens: i64,
    pub zones: i64,
    pub ventilation: VentilationParSource,
}

impl ComptesStats {
    pub(crate) fn compter(state: &AppState) -> Self {
        Self {
            artists: ArtistRepo::with_backend(state.backend.clone())
                .count()
                .unwrap_or(0),
            albums: AlbumRepo::with_backend(state.backend.clone())
                .count()
                .unwrap_or(0),
            tracks: TrackRepo::with_backend(state.backend.clone())
                .count()
                .unwrap_or(0),
            listens: HistoryRepo::with_backend(state.backend.clone())
                .count()
                .unwrap_or(0),
            zones: ZoneRepo::with_backend(state.backend.clone())
                .count()
                .unwrap_or(0),
            ventilation: VentilationParSource::lire(state),
        }
    }

    fn vides() -> Self {
        Self {
            artists: 0,
            albums: 0,
            tracks: 0,
            listens: 0,
            zones: 0,
            ventilation: VentilationParSource::vide(),
        }
    }
}

/// Les deux caches, portés par l'état : chaque état a sa base, et un test ne
/// pollue pas le suivant (même règle que `comptes_collections`).
#[derive(Default)]
pub(crate) struct Comptages {
    pub(crate) sante: Arc<CacheDeComptage<ComptesSante>>,
    pub(crate) stats: Arc<CacheDeComptage<ComptesStats>>,
    /// #9116 — les jauges d'avancement, figées pendant la pause.
    pub(crate) releves: super::avancement_suspendu_9116::RelevesSuspendus,
}

/// Les comptes de santé, hors exécuteur. Une tâche perdue se lit comme une
/// panne des deux sondes — la route le dit dans `components`.
pub(crate) async fn comptes_sante(state: &AppState, route: &'static str) -> ComptesSante {
    let s = state.clone();
    state
        .comptages
        .sante
        .lire(route, move || ComptesSante::compter(&s))
        .await
        .unwrap_or_else(ComptesSante::en_echec)
}

/// Les comptes de `/system/stats`, hors exécuteur. Une tâche perdue rend des
/// zéros, comme une erreur SQL le faisait déjà (`unwrap_or(0)`).
pub(crate) async fn comptes_stats(state: &AppState) -> ComptesStats {
    let s = state.clone();
    state
        .comptages
        .stats
        .lire("system_stats", move || ComptesStats::compter(&s))
        .await
        .unwrap_or_else(|_| ComptesStats::vides())
}

/// Une lecture synchrone courte (un réglage) hors exécuteur ; `defaut` si la
/// tâche est perdue.
pub(crate) async fn lecture_hors_executeur<T, F>(route: &'static str, defaut: T, lire: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(lire).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(route, error = %e, "lecture_tache_bloquante_perdue_8805");
            defaut
        }
    }
}

#[cfg(test)]
#[path = "comptages_8805_tests.rs"]
mod tests;
