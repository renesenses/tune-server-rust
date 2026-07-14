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
                },
            );
        }
        self.emit();
        TaskGuard {
            tasks: self.clone(),
            id,
        }
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
