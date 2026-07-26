use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::event_bus::{EventBus, TuneEvent};
use crate::outputs::traits::OutputTarget;

/// The plugin ABI generation. A plugin declares the version it was built
/// against via [`TunePlugin::protocol_version`]; [`PluginLoader::setup_all`]
/// refuses to load a plugin whose major version differs.
///
/// Bump the major on any breaking change to [`TunePlugin`] or
/// [`PluginContext`]; bump the minor when adding a backward-compatible hook.
/// The Python host had the same constant (`PROTOCOL_VERSION`) but only
/// *warned* on mismatch — here it is enforced, because a Rust plugin that
/// disagrees about the trait layout is a crash, not a degraded feature.
pub const PLUGIN_PROTOCOL_VERSION: (u32, u32) = (1, 0);

/// A zone a plugin wants the host to create on its behalf.
///
/// Plugins that expose a virtual output (a visualiser, an EQ, a stats
/// exporter) generally
/// want a zone pointing at it to exist so the device is selectable in the UI
/// without the user hand-crafting one.
#[derive(Debug, Clone)]
pub struct ZoneRequest {
    pub name: String,
    pub output_type: String,
    pub device_id: String,
}

/// Everything a plugin asked the host to install during `setup`.
///
/// [`PluginContext`] only *collects* these — it holds no lock on the output
/// registry and never touches the axum router, so a plugin's `setup` can
/// never deadlock against the host's startup path. The host drains this
/// afterwards via [`PluginLoader::take_registrations`] and applies it all at
/// once, at a point where it knows the registry and router are free.
#[derive(Default)]
pub struct PluginRegistrations {
    pub outputs: Vec<Box<dyn OutputTarget>>,
    /// `(plugin name, router)`. The host derives the mount path from the
    /// name — plugins do not choose their own prefix. Requires the
    /// `plugin-http` feature.
    #[cfg(feature = "plugin-http")]
    pub routers: Vec<(String, axum::Router<()>)>,
    pub zones: Vec<ZoneRequest>,
}

impl PluginRegistrations {
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "plugin-http")]
        if !self.routers.is_empty() {
            return false;
        }
        self.outputs.is_empty() && self.zones.is_empty()
    }

    fn absorb(&mut self, other: PluginRegistrations) {
        self.outputs.extend(other.outputs);
        #[cfg(feature = "plugin-http")]
        self.routers.extend(other.routers);
        self.zones.extend(other.zones);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub config_schema: serde_json::Value,
}

pub struct PluginContext {
    pub api_base_url: String,
    pub data_dir: PathBuf,
    pub event_bus: Option<EventBus>,
    plugin_name: String,
    db: Option<Arc<dyn DbBackend>>,
    /// Deferred registrations collected during `setup`. Interior mutability so
    /// plugins receive `&PluginContext` (not `&mut`) and can register from
    /// inside closures without fighting the borrow checker.
    registrations: StdMutex<PluginRegistrations>,
}

impl PluginContext {
    pub fn new(api_base_url: &str, data_dir: PathBuf) -> Self {
        Self {
            api_base_url: api_base_url.to_string(),
            data_dir,
            event_bus: None,
            plugin_name: String::new(),
            db: None,
            registrations: StdMutex::new(PluginRegistrations::default()),
        }
    }

    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_db(mut self, db: Arc<dyn DbBackend>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_plugin_name(mut self, name: &str) -> Self {
        self.plugin_name = name.to_string();
        self
    }

    /// Read a plugin-specific setting from the database.
    ///
    /// Keys are stored under the prefix `plugin_{name}_{key}` in the
    /// settings table, matching the convention used by the REST routes.
    pub fn get_config(&self, key: &str) -> Option<String> {
        let db = self.db.as_ref()?;
        let repo = SettingsRepo::with_backend(Arc::clone(db));
        let full_key = format!("plugin_{}_{}", self.plugin_name, key);
        repo.get(&full_key).ok().flatten()
    }

    /// Write a plugin-specific setting to the database.
    ///
    /// Keys are stored under the prefix `plugin_{name}_{key}`.
    pub fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("no database backend")?;
        let repo = SettingsRepo::with_backend(Arc::clone(db));
        let full_key = format!("plugin_{}_{}", self.plugin_name, key);
        repo.set(&full_key, value)
    }

    /// Emit an event through the event bus (if available).
    pub fn emit_event(&self, event_type: &str, data: Value) {
        if let Some(bus) = &self.event_bus {
            bus.emit(event_type, data);
        }
    }

    /// The database backend, for plugins that need to query the library
    /// directly (e.g. a plugin skipping albums already in the library).
    pub fn db(&self) -> Option<Arc<dyn DbBackend>> {
        self.db.clone()
    }

    /// The name this plugin was registered under. Also the leaf of `data_dir`.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Expose an audio output. The host registers it with the
    /// `OutputRegistry` after `setup` returns, keyed on `device_id()`.
    ///
    /// This is the Rust counterpart of the Python host's
    /// `register_output_type`. The shape differs deliberately: Python
    /// registered a *factory* keyed by type name and instantiated one output
    /// per zone, whereas `OutputRegistry` is keyed by `device_id`, so a plugin
    /// registers concrete instances. A plugin wanting N outputs calls this N
    /// times.
    pub fn register_output(&self, output: Box<dyn OutputTarget>) {
        if let Ok(mut reg) = self.registrations.lock() {
            reg.outputs.push(output);
        }
    }

    /// Expose HTTP routes. The host mounts them under
    /// `/api/v1/ext/{plugin_name}`, behind the same auth, analytics and
    /// body-limit layers as the rest of `/api/v1`.
    ///
    /// The plugin does **not** choose its own prefix — deliberately. The
    /// Python host let plugins mount anywhere, which let a plugin shadow a
    /// core route (or another plugin's) with no diagnostic. Deriving the
    /// namespace from the plugin name makes collisions impossible and keeps
    /// plugin routes obvious in a request log.
    ///
    /// The router is `Router<()>`: plugins capture their own state in
    /// closures rather than sharing the host's `AppState`, which keeps
    /// `tune-core` free of any dependency on `tune-server`'s types.
    #[cfg(feature = "plugin-http")]
    pub fn register_router(&self, router: axum::Router<()>) {
        if let Ok(mut reg) = self.registrations.lock() {
            reg.routers.push((self.plugin_name.clone(), router));
        }
    }

    /// Ask the host to create a zone bound to one of this plugin's outputs,
    /// if no zone already targets that `device_id`.
    pub fn register_zone(&self, name: &str, output_type: &str, device_id: &str) {
        if let Ok(mut reg) = self.registrations.lock() {
            reg.zones.push(ZoneRequest {
                name: name.to_string(),
                output_type: output_type.to_string(),
                device_id: device_id.to_string(),
            });
        }
    }

    /// Drain what this plugin registered. Called by the loader once `setup`
    /// has returned successfully; a plugin whose setup failed is never
    /// drained, so its half-built outputs are dropped rather than installed.
    fn take_registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }
}

#[async_trait]
pub trait TunePlugin: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn description(&self) -> &str;
    fn config_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    /// The [`PLUGIN_PROTOCOL_VERSION`] this plugin was built against.
    ///
    /// Defaults to the version compiled into the SDK the plugin links, which
    /// is correct for in-tree plugins. Override only to pin an older
    /// generation deliberately.
    fn protocol_version(&self) -> (u32, u32) {
        PLUGIN_PROTOCOL_VERSION
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String>;
    async fn teardown(&mut self) -> Result<(), String>;

    /// Called when the event bus emits an event.
    /// Override to react to playback, library, or system events.
    async fn on_event(&mut self, _event: &TuneEvent) {}

    /// Read plugin-specific configuration from the context data_dir.
    fn read_config(&self, ctx: &PluginContext) -> serde_json::Value {
        let path = ctx.data_dir.join("config.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write plugin-specific configuration to the context data_dir.
    fn write_config(&self, ctx: &PluginContext, config: &serde_json::Value) -> Result<(), String> {
        let path = ctx.data_dir.join("config.json");
        let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }
}

pub struct PluginLoader {
    plugins: Arc<tokio::sync::Mutex<Vec<Box<dyn TunePlugin>>>>,
    data_root: PathBuf,
    event_bus: Option<EventBus>,
    db: Option<Arc<dyn DbBackend>>,
    event_dispatch_handle: Option<tokio::task::JoinHandle<()>>,
    /// Registrations accumulated across every plugin's `setup`, awaiting
    /// collection by the host.
    registrations: StdMutex<PluginRegistrations>,
}

impl PluginLoader {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            plugins: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            data_root,
            event_bus: None,
            db: None,
            event_dispatch_handle: None,
            registrations: StdMutex::new(PluginRegistrations::default()),
        }
    }

    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_db(mut self, db: Arc<dyn DbBackend>) -> Self {
        self.db = Some(db);
        self
    }

    pub async fn register(&self, plugin: Box<dyn TunePlugin>) {
        self.plugins.lock().await.push(plugin);
    }

    pub async fn setup_all(&self, api_base_url: &str) -> Vec<String> {
        let mut loaded = Vec::new();
        std::fs::create_dir_all(&self.data_root).ok();

        let mut plugins = self.plugins.lock().await;
        for plugin in plugins.iter_mut() {
            let name = plugin.name().to_string();

            // Runtime kill switch written by the REST routes
            // (`plugin_{name}_enabled`): a compiled-in plugin can be disabled
            // without recompiling (review #907).
            if let Some(db) = &self.db {
                let key = format!("plugin_{name}_enabled");
                if let Ok(Some(v)) = SettingsRepo::with_backend(Arc::clone(db)).get(&key) {
                    if v == "false" {
                        info!(plugin_name = %name, "plugin_disabled_by_setting");
                        continue;
                    }
                }
            }

            // ABI gate. A plugin built against a different major generation
            // disagrees about the trait layout, so refuse it outright rather
            // than let it fault at the first dispatch.
            let (want_major, want_minor) = plugin.protocol_version();
            let (have_major, have_minor) = PLUGIN_PROTOCOL_VERSION;
            if want_major != have_major {
                warn!(
                    plugin_name = %name,
                    plugin_protocol = format!("{want_major}.{want_minor}"),
                    server_protocol = format!("{have_major}.{have_minor}"),
                    "plugin_protocol_incompatible"
                );
                continue;
            }
            if want_minor > have_minor {
                warn!(
                    plugin_name = %name,
                    plugin_protocol = format!("{want_major}.{want_minor}"),
                    server_protocol = format!("{have_major}.{have_minor}"),
                    "plugin_protocol_newer_than_server"
                );
                continue;
            }

            let data_dir = self.data_root.join(&name);
            std::fs::create_dir_all(&data_dir).ok();

            let mut ctx = PluginContext::new(api_base_url, data_dir).with_plugin_name(&name);
            if let Some(bus) = &self.event_bus {
                ctx = ctx.with_event_bus(bus.clone());
            }
            if let Some(db) = &self.db {
                ctx = ctx.with_db(Arc::clone(db));
            }

            match plugin.setup(&ctx).await {
                Ok(()) => {
                    let reg = ctx.take_registrations();
                    #[cfg(feature = "plugin-http")]
                    let router_count = reg.routers.len();
                    #[cfg(not(feature = "plugin-http"))]
                    let router_count = 0usize;
                    info!(
                        plugin_name = %name,
                        version = %plugin.version(),
                        outputs = reg.outputs.len(),
                        routers = router_count,
                        zones = reg.zones.len(),
                        "plugin_loaded"
                    );
                    if let Ok(mut acc) = self.registrations.lock() {
                        acc.absorb(reg);
                    }
                    loaded.push(name);
                }
                Err(e) => {
                    // Deliberately not draining ctx here: a plugin that failed
                    // halfway may have registered an output backed by
                    // half-initialised state. Dropping it is the safe move.
                    warn!(plugin_name = %name, error = %e, "plugin_setup_failed");
                }
            }
        }

        // Drop refused/failed/disabled plugins from the resident set: they
        // would otherwise show up as loaded in /api/v1/plugins and keep
        // receiving every event via on_event on half-built state — the very
        // hazard setup registrations are dropped for (review #907).
        plugins.retain(|p| loaded.iter().any(|n| n == p.name()));

        loaded
    }

    /// Take everything the loaded plugins asked the host to install.
    ///
    /// Call once, after [`setup_all`](Self::setup_all). Returns an empty set
    /// on subsequent calls.
    pub fn take_registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }

    /// Start dispatching EventBus events to all loaded plugins.
    ///
    /// Spawns a background task that subscribes to the event bus and forwards
    /// every event to each plugin's `on_event` callback.  Call this **after**
    /// `setup_all`.  The dispatch task runs until `teardown_all` is called.
    pub fn start_event_dispatch(&mut self) {
        let bus = match &self.event_bus {
            Some(b) => b.clone(),
            None => return,
        };

        let plugins = Arc::clone(&self.plugins);
        let mut rx = bus.subscribe();

        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let mut locked = plugins.lock().await;
                        for plugin in locked.iter_mut() {
                            plugin.on_event(&event).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "plugin_event_dispatch_lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        self.event_dispatch_handle = Some(handle);
    }

    pub async fn teardown_all(&mut self) {
        // Stop the dispatch task first.
        if let Some(handle) = self.event_dispatch_handle.take() {
            handle.abort();
            let _ = handle.await;
        }

        let mut plugins = self.plugins.lock().await;
        for plugin in plugins.iter_mut().rev() {
            let name = plugin.name().to_string();
            if let Err(e) = plugin.teardown().await {
                warn!(plugin_name = %name, error = %e, "plugin_teardown_failed");
            }
        }
        plugins.clear();
    }

    pub async fn loaded_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .lock()
            .await
            .iter()
            .map(|p| PluginInfo {
                name: p.name().to_string(),
                version: p.version().to_string(),
                description: p.description().to_string(),
                enabled: true,
                config_schema: p.config_schema(),
            })
            .collect()
    }

    pub async fn plugin_count(&self) -> usize {
        self.plugins.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TestPlugin {
        setup_called: bool,
        teardown_called: bool,
    }

    impl TestPlugin {
        fn new() -> Self {
            Self {
                setup_called: false,
                teardown_called: false,
            }
        }
    }

    #[async_trait]
    impl TunePlugin for TestPlugin {
        fn name(&self) -> &str {
            "test-plugin"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "A test plugin"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            self.setup_called = true;
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            self.teardown_called = true;
            Ok(())
        }
    }

    struct FailingPlugin;

    #[async_trait]
    impl TunePlugin for FailingPlugin {
        fn name(&self) -> &str {
            "failing"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Err("setup error".into())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Plugin that records every event it receives.
    struct EventRecorderPlugin {
        events: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl EventRecorderPlugin {
        fn new(events: Arc<tokio::sync::Mutex<Vec<String>>>) -> Self {
            Self { events }
        }
    }

    #[async_trait]
    impl TunePlugin for EventRecorderPlugin {
        fn name(&self) -> &str {
            "event-recorder"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Records events for testing"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
        async fn on_event(&mut self, event: &TuneEvent) {
            self.events.lock().await.push(event.event_type.clone());
        }
    }

    #[tokio::test]
    async fn loader_setup_and_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
        assert_eq!(loader.plugin_count().await, 1);

        let info = loader.loaded_plugins().await;
        assert_eq!(info[0].name, "test-plugin");
        assert_eq!(info[0].version, "0.1.0");

        loader.teardown_all().await;
        assert_eq!(loader.plugin_count().await, 0);
    }

    #[tokio::test]
    async fn failing_plugin_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailingPlugin)).await;
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);

        // The failed plugin must not linger: not reported as loaded, and no
        // longer resident to receive events on half-built state.
        let infos = loader.loaded_plugins().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "test-plugin");
        assert_eq!(loader.plugin_count().await, 1);
    }

    #[test]
    fn plugin_context_basic() {
        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp/test"));
        assert_eq!(ctx.api_base_url, "http://localhost");
        assert!(ctx.event_bus.is_none());
    }

    #[tokio::test]
    async fn empty_loader() {
        let loader = PluginLoader::new(PathBuf::from("/tmp"));
        assert_eq!(loader.plugin_count().await, 0);
        assert!(loader.loaded_plugins().await.is_empty());
    }

    #[test]
    fn plugin_context_emit_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp")).with_event_bus(bus);

        ctx.emit_event("test.event", json!({"key": "value"}));

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event_type, "test.event");
        assert_eq!(event.data["key"], "value");
    }

    #[test]
    fn plugin_context_config_with_db() {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp"))
            .with_plugin_name("myplugin")
            .with_db(Arc::clone(&backend));

        assert!(ctx.get_config("volume").is_none());

        ctx.set_config("volume", "80").unwrap();
        assert_eq!(ctx.get_config("volume").unwrap(), "80");

        // Verify key is namespaced in the DB.
        let repo = SettingsRepo::with_backend(backend);
        assert_eq!(repo.get("plugin_myplugin_volume").unwrap().unwrap(), "80");
    }

    /// Plugin that exercises the whole registration surface.
    struct RegisteringPlugin {
        protocol: (u32, u32),
    }

    #[async_trait]
    impl TunePlugin for RegisteringPlugin {
        fn name(&self) -> &str {
            "registering"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Registers an output, a router and a zone"
        }
        fn protocol_version(&self) -> (u32, u32) {
            self.protocol
        }
        async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
            ctx.register_output(Box::new(crate::outputs::mock::MockOutput::new(
                "plug:1", "Plugged",
            )));
            #[cfg(feature = "plugin-http")]
            ctx.register_router(axum::Router::new());
            ctx.register_zone("Plugged", "mock", "plug:1");
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Registers an output and *then* fails — the host must not install it.
    struct FailsAfterRegistering;

    #[async_trait]
    impl TunePlugin for FailsAfterRegistering {
        fn name(&self) -> &str {
            "half-built"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Registers then fails"
        }
        async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
            ctx.register_output(Box::new(crate::outputs::mock::MockOutput::new(
                "ghost:1", "Ghost",
            )));
            Err("blew up after registering".into())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn setup_collects_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader
            .register(Box::new(RegisteringPlugin {
                protocol: PLUGIN_PROTOCOL_VERSION,
            }))
            .await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["registering"]);

        let reg = loader.take_registrations();
        assert_eq!(reg.outputs.len(), 1);
        assert_eq!(reg.outputs[0].device_id(), "plug:1");
        #[cfg(feature = "plugin-http")]
        {
            assert_eq!(reg.routers.len(), 1);
            // The name is stamped by the context, not chosen by the plugin.
            assert_eq!(reg.routers[0].0, "registering");
        }
        assert_eq!(reg.zones.len(), 1);
        assert_eq!(reg.zones[0].device_id, "plug:1");

        // Draining is one-shot.
        assert!(loader.take_registrations().is_empty());
    }

    #[tokio::test]
    async fn failed_setup_discards_its_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailsAfterRegistering)).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert!(loaded.is_empty());
        // The output it managed to register before failing must not reach the
        // host — otherwise a broken plugin leaves a zombie device selectable
        // in the UI.
        assert!(loader.take_registrations().is_empty());
    }

    #[tokio::test]
    async fn incompatible_protocol_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        let (major, minor) = PLUGIN_PROTOCOL_VERSION;

        // Different major: refused.
        loader
            .register(Box::new(RegisteringPlugin {
                protocol: (major + 1, 0),
            }))
            .await;
        assert!(loader.setup_all("http://localhost:8888").await.is_empty());
        assert!(loader.take_registrations().is_empty());

        // Newer minor than the server implements: also refused, since the
        // plugin may call a hook this server does not have.
        let loader2 = PluginLoader::new(dir.path().to_path_buf());
        loader2
            .register(Box::new(RegisteringPlugin {
                protocol: (major, minor + 1),
            }))
            .await;
        assert!(loader2.setup_all("http://localhost:8888").await.is_empty());

        // Older minor: accepted.
        let loader3 = PluginLoader::new(dir.path().to_path_buf());
        loader3
            .register(Box::new(RegisteringPlugin {
                protocol: (major, minor),
            }))
            .await;
        assert_eq!(
            loader3.setup_all("http://localhost:8888").await,
            vec!["registering"]
        );
    }

    #[tokio::test]
    async fn event_dispatch_forwards_to_plugins() {
        let bus = EventBus::new();
        let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));

        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf()).with_event_bus(bus.clone());

        loader
            .register(Box::new(EventRecorderPlugin::new(Arc::clone(&events))))
            .await;
        loader.setup_all("http://localhost:8888").await;
        loader.start_event_dispatch();

        // Emit an event and give the dispatch task time to process it.
        bus.emit("playback.started", json!({}));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let recorded = events.lock().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], "playback.started");
    }
}
