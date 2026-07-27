//! P2 of the plugin ABI (`docs/plugins/PLUGIN_ABI_RFC.md`, §3.4–3.6): the
//! **real** host wiring behind the P1 [`HostContext`] seam, backed by
//! [`AppState`], plus the registry of loaded wasm plugins the route mount
//! ([`crate::routes::plugins`]) dispatches into.
//!
//! P0/P1 (in `tune-core::plugins_runtime`) left the host side abstract behind
//! [`HostContext`] so it was unit-testable with a mock. This module provides
//! the concrete implementation: [`AppStateHost`] forwards each capability to
//! the same repos/orchestrator/event-bus the REST routes use, so a plugin's
//! `host_queue_add` lands in the *actual* play queue and its `host_now_playing`
//! reads the *actual* zone state.
//!
//! # Async bridge
//!
//! [`HostContext`]'s methods are synchronous (they are invoked from inside a
//! wasm call, deep in a non-async wasmtime call stack), but three of them
//! (`now_playing`, `play`, `pause`) need the async orchestrator/playback API.
//! We bridge with [`tokio::runtime::Handle::block_on`], which is only sound off
//! a runtime worker thread — so the route handler always drives the wasm call
//! (and therefore any host-function it triggers) inside
//! [`tokio::task::spawn_blocking`]. The queue/log/emit capabilities are pure
//! sync (rusqlite / tracing / the event bus) and need no bridge.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{debug, error, info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::event_bus::EventBus;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::PlaybackManager;
use tune_core::plugins::{PluginManager, PluginManifest};
use tune_core::plugins_runtime::{HostContext, Limits, WasmPlugin};

use crate::state::AppState;

/// Run an async future to completion from a synchronous host-function.
///
/// Sound only when called off a tokio runtime worker (a runtime worker would
/// panic). The P2 route handler guarantees this by driving every wasm call —
/// and hence every host-function it triggers — inside `spawn_blocking`.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(fut)
}

/// The concrete [`HostContext`]: the plugin capability surface backed by the
/// live server. Holds `Arc`-clones of exactly the pieces of [`AppState`] the
/// P1 capabilities need — no reference back to `AppState` itself, so it is
/// cheap to build and free of cycles.
pub struct AppStateHost {
    backend: Arc<dyn DbBackend>,
    playback: Arc<PlaybackManager>,
    orchestrator: Arc<PlaybackOrchestrator>,
    event_bus: Arc<EventBus>,
}

impl AppStateHost {
    /// Build a host from the server state, cloning the `Arc`s it forwards to.
    pub fn from_state(state: &AppState) -> Self {
        Self {
            backend: state.backend.clone(),
            playback: state.playback.clone(),
            orchestrator: state.orchestrator.clone(),
            event_bus: state.event_bus.clone(),
        }
    }

    /// The zone's assigned output device id, if any — network capabilities
    /// (`play`/`pause`) need it to reach the renderer, mirroring the REST routes.
    fn zone_device_id(&self, zone: i64) -> Option<String> {
        ZoneRepo::with_backend(self.backend.clone())
            .get(zone)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    }
}

impl HostContext for AppStateHost {
    fn log(&self, level: &str, msg: &str) {
        match level {
            "error" => error!(target: "plugin", "{msg}"),
            "warn" => warn!(target: "plugin", "{msg}"),
            "debug" | "trace" => debug!(target: "plugin", "{msg}"),
            _ => info!(target: "plugin", "{msg}"),
        }
    }

    fn queue_get(&self, zone: i64) -> Result<Value, String> {
        let entries = PlayQueueRepo::with_backend(self.backend.clone()).get_ordered(zone)?;
        let position = entries
            .iter()
            .position(|e| e.is_current)
            .map(|p| p as i64)
            .unwrap_or(0);
        Ok(json!({
            "zone": zone,
            "length": entries.len(),
            "position": position,
            "tracks": entries,
        }))
    }

    fn queue_add(&self, zone: i64, tracks: Value) -> Result<Value, String> {
        // Accept either a bare array of items or `{ "tracks": [...] }`; each
        // item is a local `{track_id}` or a streaming `{source, source_id, …}`.
        let items = match &tracks {
            Value::Array(a) => a.clone(),
            Value::Object(_) => tracks
                .get("tracks")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        let mut inputs: Vec<QueueInput> = Vec::new();
        for t in &items {
            if let Some(track_id) = t.get("track_id").and_then(Value::as_i64) {
                inputs.push(QueueInput::Local { track_id });
            } else if let (Some(source), Some(source_id)) = (
                t.get("source").and_then(Value::as_str),
                t.get("source_id").and_then(Value::as_str),
            ) {
                inputs.push(QueueInput::Streaming {
                    source: source.to_string(),
                    source_id: source_id.to_string(),
                    title: t
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    artist: t
                        .get("artist")
                        .or_else(|| t.get("artist_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    album: t
                        .get("album")
                        .or_else(|| t.get("album_title"))
                        .and_then(Value::as_str)
                        .map(String::from),
                    cover_url: t
                        .get("cover_url")
                        .or_else(|| t.get("cover_path"))
                        .and_then(Value::as_str)
                        .map(String::from),
                    duration_ms: t.get("duration_ms").and_then(Value::as_i64).unwrap_or(0),
                });
            }
        }

        if inputs.is_empty() {
            return Err("queue_add: no valid tracks (need track_id or source+source_id)".into());
        }

        let added = inputs.len();
        PlayQueueRepo::with_backend(self.backend.clone()).append(zone, &inputs)?;
        Ok(json!({ "ok": true, "added": added }))
    }

    fn now_playing(&self, zone: i64) -> Result<Value, String> {
        let state = block_on(self.playback.get_state(zone));
        serde_json::to_value(&state).map_err(|e| format!("serialise zone state: {e}"))
    }

    fn play(&self, zone: i64, req: Value) -> Result<Value, String> {
        let output_device_id = req
            .get("output_device_id")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| self.zone_device_id(zone));
        let orch_req = PlayRequest {
            zone_id: zone,
            output_device_id,
            track_id: req.get("track_id").and_then(Value::as_i64),
            source: req.get("source").and_then(Value::as_str).map(String::from),
            source_id: req
                .get("source_id")
                .and_then(Value::as_str)
                .map(String::from),
            title: req.get("title").and_then(Value::as_str).map(String::from),
            artist_name: req
                .get("artist_name")
                .and_then(Value::as_str)
                .map(String::from),
            album_title: req
                .get("album_title")
                .and_then(Value::as_str)
                .map(String::from),
            cover_url: req
                .get("cover_url")
                .and_then(Value::as_str)
                .map(String::from),
            duration_ms: req.get("duration_ms").and_then(Value::as_i64),
            ..Default::default()
        };
        let result = block_on(self.orchestrator.play(orch_req))?;
        Ok(json!({
            "output_sent": result.output_sent,
            "source": result.source,
            "stream_url": result.stream_url,
            "error": result.error,
        }))
    }

    fn pause(&self, zone: i64) -> Result<Value, String> {
        let device_id = self.zone_device_id(zone);
        block_on(self.orchestrator.pause(zone, device_id.as_deref()));
        Ok(json!({ "ok": true }))
    }

    fn emit(&self, event: &str, payload: Value) {
        self.event_bus.emit(event, payload);
    }
}

/// A wasm plugin that loaded successfully, plus the manifest the route mount
/// needs (premium flag, id). The [`WasmPlugin`] owns a wasmtime `Store` and so
/// is **not** `Sync`; the `Mutex` serialises calls per plugin, as the RFC
/// requires (one `Store`/`Instance` per active plugin).
pub struct LoadedWasmPlugin {
    pub manifest: PluginManifest,
    pub plugin: tokio::sync::Mutex<WasmPlugin>,
}

/// The set of loaded wasm plugins, keyed by manifest id. Published once into
/// [`AppState::wasm_plugins`] at startup and read (never mutated) by the route
/// handler — mirroring the `plugin_info` snapshot pattern.
pub struct WasmRegistry {
    plugins: HashMap<String, LoadedWasmPlugin>,
}

impl WasmRegistry {
    /// Look up a loaded plugin by manifest id.
    pub fn get(&self, id: &str) -> Option<&LoadedWasmPlugin> {
        self.plugins.get(id)
    }

    /// Number of loaded plugins (diagnostics/tests).
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether no plugin loaded.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Directory the manifests live under: `{TUNE_PLUGINS_DIR}/{id}/manifest.json`,
/// defaulting to `plugins/`. (`TUNE_PLUGINS_DATA_DIR` is the *data* root used by
/// the SDK loader — a different concern.)
fn plugins_dir() -> PathBuf {
    std::env::var("TUNE_PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("plugins"))
}

/// Scan the plugins directory and load every **enabled** wasm plugin whose
/// `entry_point` file exists, wiring each to an [`AppStateHost`] and its
/// manifest permissions, then publish the registry into `state.wasm_plugins`.
///
/// A plugin is enabled unless its `plugin_{id}_enabled` setting is explicitly
/// `"false"` (the same key the REST enable/disable handlers write). A plugin
/// that fails to load is logged and skipped — a bad plugin must never crash
/// startup (RFC §3.7). Idempotent-ish: safe to call once; a second call is a
/// no-op because the `OnceLock` is already set.
pub async fn load_wasm_plugins(state: &AppState) {
    let dir = plugins_dir();
    let manager = PluginManager::new(dir.clone());
    let infos = match manager.scan().await {
        Ok(infos) => infos,
        Err(e) => {
            // A missing dir returns Ok(empty); this is a real read error.
            warn!(dir = %dir.display(), error = %e, "wasm_plugins_scan_failed");
            let _ = state.wasm_plugins.set(WasmRegistry {
                plugins: HashMap::new(),
            });
            return;
        }
    };

    let host: Arc<dyn HostContext> = Arc::new(AppStateHost::from_state(state));
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut plugins: HashMap<String, LoadedWasmPlugin> = HashMap::new();

    for info in infos {
        let id = info.manifest.id.clone();

        let enabled = settings
            .get(&format!("plugin_{id}_enabled"))
            .ok()
            .flatten()
            .map(|v| v != "false")
            .unwrap_or(true);
        if !enabled {
            debug!(id = %id, "wasm_plugin_skipped_disabled");
            continue;
        }

        let entry = Path::new(&info.path).join(&info.manifest.entry_point);
        if !entry.exists() {
            warn!(id = %id, entry = %entry.display(), "wasm_plugin_entry_missing");
            continue;
        }

        let permissions: HashSet<String> = info.manifest.permissions.iter().cloned().collect();
        match WasmPlugin::load_with_host(&entry, Limits::default(), host.clone(), permissions) {
            Ok(plugin) => {
                info!(
                    id = %id,
                    permissions = ?info.manifest.permissions,
                    premium = info.manifest.premium,
                    "wasm_plugin_loaded"
                );
                plugins.insert(
                    id,
                    LoadedWasmPlugin {
                        manifest: info.manifest,
                        plugin: tokio::sync::Mutex::new(plugin),
                    },
                );
            }
            Err(e) => {
                // Never propagate: a misbehaving plugin must not crash startup.
                warn!(id = %id, error = %e, "wasm_plugin_load_failed");
            }
        }
    }

    info!(count = plugins.len(), "wasm_plugins_ready");
    let _ = state.wasm_plugins.set(WasmRegistry { plugins });
}

/// Does an event-subscription glob `pattern` match the event `name`?
///
/// The tiny grammar the manifest supports (RFC §3.6):
/// * `"*"` — everything;
/// * `"prefix.*"` — any event whose name starts with `prefix.` (the wildcard
///   also matches `prefix` itself, so `"playback.*"` catches `playback`);
/// * anything else — an exact, case-sensitive match.
fn event_matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        // `playback.*` matches `playback.state_changed` and bare `playback`.
        return name == prefix || name.starts_with(&format!("{prefix}."));
    }
    pattern == name
}

/// Whether any of a plugin's `event_subscriptions` matches `name`.
fn any_subscription_matches(subscriptions: &[String], name: &str) -> bool {
    subscriptions.iter().any(|p| event_matches(p, name))
}

/// Per-event budget: forwarding to one plugin can never block the bus longer
/// than this. A slow/looping `plugin_on_event` is abandoned (logged) so a
/// misbehaving plugin cannot stall event delivery to the others.
const ON_EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// P3 of the plugin ABI (RFC §3.6): forward `event_bus` events to every loaded
/// wasm plugin whose `event_subscriptions` glob-matches the event name, via the
/// plugin's optional `plugin_on_event` export.
///
/// Spawns a single background task that subscribes to the bus once and, for
/// each event, fans out to the matching plugins. Each plugin call is
/// serialised through the plugin's `tokio::sync::Mutex` (its wasmtime `Store`
/// is not `Sync`), driven on `spawn_blocking` (the store may `block_on` host
/// capabilities off a runtime worker), and bounded by [`ON_EVENT_TIMEOUT`].
/// **Every failure is logged and swallowed** — a plugin must never break the
/// bus (RFC §3.6/§3.7). Broadcast lag drops events silently, matching every
/// other bus consumer.
///
/// Call once, right after [`load_wasm_plugins`], from `run.rs`/`bootstrap.rs`.
pub fn spawn_wasm_event_forwarder(state: &AppState) {
    let wasm_plugins = state.wasm_plugins.clone();
    let mut rx = state.event_bus.subscribe();

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        loop {
            let event = match rx.recv().await {
                Ok(ev) => ev,
                // Lagged: some events were dropped for this slow consumer; keep
                // going with the next one (same policy as the other consumers).
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "wasm_event_forwarder_lagged");
                    continue;
                }
                // Bus closed (shutdown): stop the task.
                Err(RecvError::Closed) => break,
            };

            // The registry is published once at startup; if it is not set yet
            // (or empty) there is nothing to forward to.
            let Some(registry) = wasm_plugins.get() else {
                continue;
            };

            // Which plugins subscribed to this event name?
            let targets: Vec<String> = registry
                .plugins
                .iter()
                .filter(|(_, loaded)| {
                    any_subscription_matches(
                        &loaded.manifest.event_subscriptions,
                        &event.event_type,
                    )
                })
                .map(|(id, _)| id.clone())
                .collect();
            if targets.is_empty() {
                continue;
            }

            // Serialise the `{name, payload}` envelope once for all targets.
            let event_json = json!({
                "name": event.event_type,
                "payload": event.data,
            })
            .to_string();

            for id in targets {
                let wasm_plugins = wasm_plugins.clone();
                let event_json = event_json.clone();
                let plugin_id = id.clone();
                // Drive the wasm call on a blocking thread (the Store isn't Sync
                // and any host-function it triggers may block_on the runtime),
                // serialised per plugin via its Mutex, and time-box it.
                let call = tokio::task::spawn_blocking(move || {
                    let registry = match wasm_plugins.get() {
                        Some(r) => r,
                        None => return Ok(()),
                    };
                    let Some(loaded) = registry.get(&plugin_id) else {
                        return Ok(());
                    };
                    let mut plugin = loaded.plugin.blocking_lock();
                    plugin.on_event(&event_json)
                });

                match tokio::time::timeout(ON_EVENT_TIMEOUT, call).await {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(e))) => {
                        warn!(id = %id, event = %event.event_type, error = %e, "wasm_plugin_on_event_error");
                    }
                    Ok(Err(e)) => {
                        warn!(id = %id, event = %event.event_type, error = %e, "wasm_plugin_on_event_task_failed");
                    }
                    Err(_) => {
                        warn!(id = %id, event = %event.event_type, timeout_ms = ON_EVENT_TIMEOUT.as_millis() as u64, "wasm_plugin_on_event_timeout");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{any_subscription_matches, event_matches};

    #[test]
    fn glob_star_matches_everything() {
        assert!(event_matches("*", "playback.state_changed"));
        assert!(event_matches("*", "library.scanned"));
    }

    #[test]
    fn glob_prefix_matches_namespace_and_bare() {
        assert!(event_matches("playback.*", "playback.state_changed"));
        assert!(event_matches("playback.*", "playback"));
        assert!(!event_matches("playback.*", "library.scanned"));
        // Must not match a longer sibling namespace by accident.
        assert!(!event_matches("play.*", "playback.state_changed"));
    }

    #[test]
    fn glob_exact_matches_only_the_name() {
        assert!(event_matches("zone.created", "zone.created"));
        assert!(!event_matches("zone.created", "zone.updated"));
    }

    #[test]
    fn any_subscription_ors_the_patterns() {
        let subs = vec!["playback.*".to_string(), "zone.created".to_string()];
        assert!(any_subscription_matches(&subs, "playback.paused"));
        assert!(any_subscription_matches(&subs, "zone.created"));
        assert!(!any_subscription_matches(&subs, "library.scanned"));
        assert!(!any_subscription_matches(&[], "playback.paused"));
    }
}
