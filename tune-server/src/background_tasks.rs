//! In-memory registry of long-running background tasks (library enrichment,
//! artwork fetching, bio sync, …) so the UI can show a "tâches de fond en cours"
//! indicator at startup and during use.
//!
//! A task registers via [`BackgroundTasks::begin`], which returns a RAII
//! [`TaskGuard`]. When the guard drops — because the spawned task future
//! completed normally, returned early, or panicked — the task is removed and a
//! `system.background_tasks` event is emitted with the current snapshot. This
//! avoids the "phantom perpetual task" trap of status flags that are set to
//! `running` but never reliably reset.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::json;
use tune_core::event_bus::EventBus;
use tune_core::taches_de_fond::{Tache, est_en_pause};

/// Granular progress for a task, when the runner reports it (e.g. artist-image
/// enrichment publishes "MusicBrainz 340/1183"). Omitted for tasks that only
/// signal presence.
#[derive(Clone, Serialize)]
pub struct TaskProgress {
    pub processed: u64,
    pub total: u64,
    /// Sub-phase label, e.g. `"MusicBrainz"` or `"Images"`.
    pub detail: String,
}

/// A single background task currently in progress.
#[derive(Clone, Serialize)]
pub struct BackgroundTask {
    /// Stable identifier (also the registry key) — e.g. `"artwork"`.
    pub id: String,
    /// Human-readable, localized label shown to the user.
    pub label: String,
    /// Coarse category — e.g. `"enrichment"`.
    pub kind: String,
    /// Optional granular progress; omitted from JSON when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    /// #6282 — la tâche est inscrite, mais GARÉE : le traitement dont elle
    /// relève ([`tache_suspendable`]) est suspendu. Les passes qui tiennent
    /// leur liste en mémoire se garent vivantes (`attendre_la_reprise`) et
    /// restent donc au registre ; sans ce champ, le bandeau « Enrichissement… »
    /// et les Réglages les disaient en cours pendant que la carte de l'écran
    /// État du serveur les disait en pause (Kimon, fil 2259). Calculé à
    /// chaque [`BackgroundTasks::snapshot`], jamais stocké.
    pub paused: bool,
}

/// Le traitement suspendable dont relève une tâche du registre, par son
/// identifiant d'inscription. `None` pour une tâche qu'aucune pause
/// n'arrête (écriture des étiquettes, paroles…).
///
/// La table est UNIQUE : l'état des cartes (`routes/system/taches_de_fond.rs`,
/// `etat_de`) et le drapeau `paused` des tâches la lisent tous deux, pour que
/// la carte et le bandeau ne puissent plus se contredire.
///
/// Chaque identifiant n'est rangé ici que si la passe consulte la pause de ce
/// traitement à ses frontières.
pub fn tache_suspendable(id: &str) -> Option<Tache> {
    match id {
        // `POST /library/enrich-all`, les biographies, la passe automatique
        // des crédits, le type de sortie (#4767) et les crédits par disque
        // (`TACHE_CREDITS_RELEASES`, #4862) : `attendre_la_reprise` /
        // `attendre_son_tour` sur `Tache::Enrichissement`.
        "enrich_all" | "bios" | "credits_enrich_auto" | "types_de_sortie" | "credits_releases" => {
            Some(Tache::Enrichissement)
        }
        // `album_artwork` : la passe des pochettes d'albums (inscrite sous
        // `artwork` avant l'écran des images, 01/11/2026) ; elle attend son
        // tour sur `Tache::ImagesArtistes`, comme les portraits.
        "artist_artwork" | "artwork" | "album_artwork" => Some(Tache::ImagesArtistes),
        // `label_logos` n'est PAS rangé ici : la passe Qobuz ne consulte
        // aucune pause, et la carte « Enrichissement » se dirait en cours
        // pendant une passe de logos.
        "identification_lot" => Some(Tache::Identification),
        // La plage dynamique à la demande (#4185).
        "dynamic_range" => Some(Tache::PlageDynamique),
        // L'audit d'intégrité (#6286) : `passe_doit_continuer` lit la pause
        // de `Tache::Integrite` entre deux paquets décodés.
        "integrity" => Some(Tache::Integrite),
        _ => None,
    }
}

/// Registry of in-progress background tasks. Cheap to clone (shared `Arc`s).
#[derive(Clone)]
pub struct BackgroundTasks {
    inner: Arc<Mutex<HashMap<String, BackgroundTask>>>,
    event_bus: Arc<EventBus>,
}

impl BackgroundTasks {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            event_bus,
        }
    }

    /// Register a running task. The returned guard removes it on drop, so the
    /// idiom is to move the guard into the spawned task:
    ///
    /// ```ignore
    /// let guard = state.background_tasks.begin("artwork", "…", "enrichment");
    /// tokio::spawn(async move {
    ///     let _guard = guard; // ends the task when this future completes
    ///     // … long-running work …
    /// });
    /// ```
    ///
    /// Re-registering the same `id` while it is already active simply refreshes
    /// its label; the first guard to drop clears it. (In practice each task type
    /// is single-flighted upstream.)
    pub fn begin(
        &self,
        id: impl Into<String>,
        label: impl Into<String>,
        kind: impl Into<String>,
    ) -> TaskGuard {
        let id = id.into();
        {
            let mut map = self.inner.lock().unwrap();
            map.insert(
                id.clone(),
                BackgroundTask {
                    id: id.clone(),
                    label: label.into(),
                    kind: kind.into(),
                    progress: None,
                    paused: false,
                },
            );
        }
        self.emit();
        TaskGuard {
            tasks: self.clone(),
            id,
        }
    }

    /// Comme [`Self::begin`], mais SEULEMENT si aucune tâche `id` n'est
    /// inscrite : `None` quand elle tourne déjà. La vérification et
    /// l'inscription se font sous le même verrou, si bien que deux clics
    /// simultanés ne lancent qu'une passe (écran « État du serveur »,
    /// 01/11/2026 : `label_logos`, `album_artwork`).
    pub fn try_begin(
        &self,
        id: impl Into<String>,
        label: impl Into<String>,
        kind: impl Into<String>,
    ) -> Option<TaskGuard> {
        let id = id.into();
        {
            let mut map = self.inner.lock().unwrap();
            if map.contains_key(&id) {
                return None;
            }
            map.insert(
                id.clone(),
                BackgroundTask {
                    id: id.clone(),
                    label: label.into(),
                    kind: kind.into(),
                    progress: None,
                    paused: false,
                },
            );
        }
        self.emit();
        Some(TaskGuard {
            tasks: self.clone(),
            id,
        })
    }

    /// La tâche `id` est-elle inscrite, et où en est-elle ?
    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// Attach/refresh granular progress on an already-registered task. No-op if
    /// the task isn't currently active (e.g. it just finished and cleared).
    pub fn update_progress(&self, id: &str, processed: u64, total: u64, detail: impl Into<String>) {
        {
            let mut map = self.inner.lock().unwrap();
            let Some(task) = map.get_mut(id) else { return };
            task.progress = Some(TaskProgress {
                processed,
                total,
                detail: detail.into(),
            });
        }
        self.emit();
    }

    /// Current tasks, sorted by id for a stable UI ordering.
    pub fn snapshot(&self) -> Vec<BackgroundTask> {
        let map = self.inner.lock().unwrap();
        let mut v: Vec<BackgroundTask> = map.values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        for t in &mut v {
            t.paused = tache_suspendable(&t.id).is_some_and(est_en_pause);
        }
        v
    }

    fn end(&self, id: &str) {
        {
            let mut map = self.inner.lock().unwrap();
            map.remove(id);
        }
        self.emit();
    }

    fn emit(&self) {
        self.event_bus.emit(
            "system.background_tasks",
            json!({ "tasks": self.snapshot() }),
        );
    }
}

/// RAII guard that ends its task when dropped. Hold it for the task's lifetime.
#[must_use = "dropping the guard immediately ends the task; hold it for the task's lifetime"]
pub struct TaskGuard {
    tasks: BackgroundTasks,
    id: String,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.tasks.end(&self.id);
    }
}
