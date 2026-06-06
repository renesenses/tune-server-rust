use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::event_bus::{EventBus, TuneEvent};

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
}

impl PluginContext {
    pub fn new(api_base_url: &str, data_dir: PathBuf) -> Self {
        Self {
            api_base_url: api_base_url.to_string(),
            data_dir,
            event_bus: None,
            plugin_name: String::new(),
            db: None,
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
}

#[async_trait]
pub trait TunePlugin: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn description(&self) -> &str;
    fn config_schema(&self) -> serde_json::Value {
        serde_json::json!({})
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
}

impl PluginLoader {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            plugins: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            data_root,
            event_bus: None,
            db: None,
            event_dispatch_handle: None,
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
                    info!(
                        plugin_name = %name,
                        version = %plugin.version(),
                        "plugin_loaded"
                    );
                    loaded.push(name);
                }
                Err(e) => {
                    warn!(plugin_name = %name, error = %e, "plugin_setup_failed");
                }
            }
        }

        loaded
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
        let mut loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailingPlugin)).await;
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
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
