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
                    track_number: t.get("track_number").and_then(Value::as_i64),
                    disc_number: t.get("disc_number").and_then(Value::as_i64),
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
        block_on(self.orchestrator.pause(zone, device_id.as_deref()))
            .map_err(|error| error.to_string())?;
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

/// Directory the manifests live under: `{TUNE_PLUGINS_DIR}/{id}/manifest.json`.
/// (`TUNE_PLUGINS_DATA_DIR` is the *data* root used by the SDK loader — a
/// different concern.)
///
/// Resolution order:
/// 1. `TUNE_PLUGINS_DIR` if set (tests set it explicitly; ops can override it).
/// 2. `plugins/` **next to the executable** if that dir exists — a shipped
///    server (systemd/Homebrew/Docker) often runs with a CWD that is *not* the
///    package dir, so a bare CWD-relative `plugins/` would silently never be
///    found and the bundled Party plugin would never load.
/// 3. CWD-relative `plugins/` otherwise (dev `cargo run` from the repo root).
pub(crate) fn plugins_dir() -> PathBuf {
    resolve_plugins_dir(
        std::env::var("TUNE_PLUGINS_DIR").ok(),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf)),
    )
}

/// Persist a downloaded marketplace archive (a zip holding at least
/// `manifest.json` + the manifest's `entry_point` wasm) into
/// `{plugins_dir}/{manifest.id}/`, where [`load_wasm_plugins`] picks it up at
/// next startup. Returns the manifest id. The manifest inside the archive is
/// authoritative — the store slug or package name never names the directory.
pub(crate) fn persist_plugin_archive(data: &[u8]) -> Result<String, String> {
    persist_plugin_archive_in(&plugins_dir(), data)
}

/// [`persist_plugin_archive`] with an explicit destination root, split out so
/// tests never depend on process-global env or the executable's location.
fn persist_plugin_archive_in(root: &Path, data: &[u8]) -> Result<String, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))
        .map_err(|e| format!("invalid plugin archive: {e}"))?;

    let manifest: PluginManifest = {
        let file = zip
            .by_name("manifest.json")
            .map_err(|_| "archive has no manifest.json".to_string())?;
        serde_json::from_reader(file).map_err(|e| format!("invalid manifest.json: {e}"))?
    };
    // The id becomes a directory name AND a URL path segment (`/plugins/{id}/…`).
    if manifest.id.is_empty()
        || !manifest
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unsafe plugin id in manifest: {:?}", manifest.id));
    }

    let dest = root.join(&manifest.id);
    std::fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;

    for i in 0..zip.len() {
        let mut file = zip
            .by_index(i)
            .map_err(|e| format!("read archive entry {i}: {e}"))?;
        if !file.is_file() {
            continue;
        }
        // enclosed_name refuses absolute paths and `..` traversal.
        let Some(rel) = file.enclosed_name() else {
            return Err(format!("unsafe path in archive: {:?}", file.name()));
        };
        let out_path = dest.join(rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .map_err(|e| format!("write {}: {e}", out_path.display()))?;
        std::io::copy(&mut file, &mut out)
            .map_err(|e| format!("write {}: {e}", out_path.display()))?;
    }

    let entry = dest.join(&manifest.entry_point);
    if !entry.exists() {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(format!(
            "archive is missing its entry point {:?}",
            manifest.entry_point
        ));
    }

    info!(id = %manifest.id, dir = %dest.display(), "plugin_archive_persisted");
    Ok(manifest.id)
}

/// Remove an installed wasm plugin's directory, if present. Returns whether a
/// directory was actually removed. The same id sanitation as install applies —
/// the id comes back from HTTP here, so it must never traverse.
pub(crate) fn remove_plugin_dir(id: &str) -> Result<bool, String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("unsafe plugin id: {id:?}"));
    }
    let dir = plugins_dir().join(id);
    if !dir.join("manifest.json").exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))?;
    info!(id = %id, dir = %dir.display(), "plugin_dir_removed");
    Ok(true)
}

/// Pure resolution logic behind [`plugins_dir`], split out so it is testable
/// without touching process-global env or the real executable path.
fn resolve_plugins_dir(env_override: Option<String>, exe_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = env_override {
        return PathBuf::from(dir);
    }
    if let Some(exe_dir) = exe_dir {
        let candidate = exe_dir.join("plugins");
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("plugins")
}

/// Env var that puts the process in probe-child mode (value = wasm path).
const WASM_PROBE_ENV: &str = "TUNE_WASM_PROBE";

/// Kill-switch: set to `1`/`true` to skip wasm plugin loading entirely.
const WASM_DISABLE_ENV: &str = "TUNE_DISABLE_WASM_PLUGINS";

/// Set to `1` to load wasm plugins WITHOUT the child-process probe.
///
/// For the integration tests, which call [`load_wasm_plugins`] from a libtest
/// binary: spawning `current_exe` there re-runs the whole test suite instead
/// of probing (libtest has no probe hook), so the probe times out and every
/// plugin is wrongly skipped. Their modules are known-good WAT anyway.
const WASM_PROBE_SKIP_ENV: &str = "TUNE_WASM_PROBE_SKIP";

/// Probe-child mode: attempt the wasmtime load of `$TUNE_WASM_PROBE`, then
/// exit 0. Survival is the whole point — a clean `Err` exits 0 too, because
/// the in-process loader handles load errors gracefully; only a process
/// *death* (SIGILL & co) tells the parent to skip wasm plugins. Must be
/// called before any server state is built; no-op outside probe mode.
pub(crate) fn maybe_run_wasm_probe() {
    if let Ok(path) = std::env::var(WASM_PROBE_ENV) {
        let _ = WasmPlugin::load(Path::new(&path), Limits::default());
        std::process::exit(0);
    }
}

/// Try compiling `entry` in a throwaway child process first.
///
/// wasmtime's codegen can die at the machine level — SIGILL on a CPU older
/// than cranelift's baseline (#1249, Cyrille's Intel Mac mini: the server
/// vanished mid-startup, log ending at `plugins_scanned`, nothing to catch
/// in-process). A child that dies proves it at zero cost; the parent skips
/// the plugin and the server lives.
fn probe_wasm_load(entry: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let child = std::process::Command::new(exe)
        .env(WASM_PROBE_ENV, entry)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    // Bounded wait: a hung probe must not hang startup either. Generous
    // deadline: a DEBUG build compiles party's wasm in ~12 s (cranelift
    // unoptimised); release builds take well under a second.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Scan the plugins directory and load every **enabled** wasm plugin whose
/// `entry_point` file exists, wiring each to an [`AppStateHost`] and its
/// manifest permissions, then publish the registry into `state.wasm_plugins`.
///
/// A plugin is enabled unless its `plugin_{id}_enabled` setting is explicitly
/// `"false"` (the same key the REST enable/disable handlers write). A plugin
/// that fails to load is logged and skipped — a bad plugin must never crash
/// startup (RFC §3.7); each load is probed in a child process first, so even
/// a machine-level death (SIGILL) skips the plugin instead of killing the
/// server. Idempotent-ish: safe to call once; a second call is a no-op
/// because the `OnceLock` is already set.
pub async fn load_wasm_plugins(state: &AppState) {
    if std::env::var(WASM_DISABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        info!("wasm_plugins_disabled_by_env");
        let _ = state.wasm_plugins.set(WasmRegistry {
            plugins: HashMap::new(),
        });
        return;
    }

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

        let skip_probe = std::env::var(WASM_PROBE_SKIP_ENV)
            .map(|v| v == "1")
            .unwrap_or(false);
        if !skip_probe && !probe_wasm_load(&entry) {
            warn!(
                id = %id,
                entry = %entry.display(),
                "wasm_plugin_probe_failed — wasmtime died loading this module \
                 on this machine (CPU below cranelift's baseline?); plugin \
                 skipped, server continues"
            );
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
    use super::{
        any_subscription_matches, event_matches, persist_plugin_archive_in, resolve_plugins_dir,
    };
    use std::io::Write;
    use std::path::PathBuf;

    /// Build an in-memory zip holding the given (name, contents) entries.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn manifest_json(id: &str) -> Vec<u8> {
        serde_json::json!({
            "id": id,
            "name": "Party Mode",
            "version": "0.1.0",
            "description": "test",
            "author": "MozAIk Labs",
            "entry_point": "main.wasm",
            "permissions": ["queue"],
            "min_server_version": "0.9.0",
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn persist_archive_writes_plugin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = make_zip(&[
            ("manifest.json", &manifest_json("party")),
            ("main.wasm", b"\0asm"),
        ]);
        let id = persist_plugin_archive_in(tmp.path(), &data).unwrap();
        assert_eq!(id, "party");
        assert!(tmp.path().join("party/manifest.json").exists());
        assert!(tmp.path().join("party/main.wasm").exists());
    }

    #[test]
    fn persist_archive_rejects_missing_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let data = make_zip(&[("manifest.json", &manifest_json("party"))]);
        let err = persist_plugin_archive_in(tmp.path(), &data).unwrap_err();
        assert!(err.contains("entry point"), "{err}");
        // The half-written directory must not survive: load_wasm_plugins would
        // warn about it at every startup.
        assert!(!tmp.path().join("party").exists());
    }

    #[test]
    fn persist_archive_rejects_unsafe_id() {
        let tmp = tempfile::tempdir().unwrap();
        let data = make_zip(&[
            ("manifest.json", &manifest_json("../evil")),
            ("main.wasm", b"\0asm"),
        ]);
        let err = persist_plugin_archive_in(tmp.path(), &data).unwrap_err();
        assert!(err.contains("unsafe plugin id"), "{err}");
    }

    #[test]
    fn persist_archive_rejects_missing_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let data = make_zip(&[("main.wasm", b"\0asm")]);
        let err = persist_plugin_archive_in(tmp.path(), &data).unwrap_err();
        assert!(err.contains("manifest.json"), "{err}");
    }

    #[test]
    fn plugins_dir_env_override_wins() {
        // An explicit TUNE_PLUGINS_DIR always wins, even when an exe-relative
        // `plugins/` exists — this is the contract the integration tests rely on.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("plugins")).unwrap();
        let resolved = resolve_plugins_dir(
            Some("/custom/plugins".to_string()),
            Some(tmp.path().to_path_buf()),
        );
        assert_eq!(resolved, PathBuf::from("/custom/plugins"));
    }

    #[test]
    fn plugins_dir_resolves_next_to_executable() {
        // With no override and an exe dir that HAS a `plugins/` subdir, resolve
        // to that absolute path (the shipped-server case).
        let tmp = tempfile::tempdir().unwrap();
        let plugins = tmp.path().join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        let resolved = resolve_plugins_dir(None, Some(tmp.path().to_path_buf()));
        assert_eq!(resolved, plugins);
    }

    #[test]
    fn plugins_dir_falls_back_to_cwd_relative() {
        // No override and no exe-relative `plugins/` → bare CWD-relative default,
        // so `cargo run` from the repo root still finds `./plugins`.
        let tmp = tempfile::tempdir().unwrap(); // exists but has no `plugins/` subdir
        assert_eq!(
            resolve_plugins_dir(None, Some(tmp.path().to_path_buf())),
            PathBuf::from("plugins")
        );
        assert_eq!(resolve_plugins_dir(None, None), PathBuf::from("plugins"));
    }

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
