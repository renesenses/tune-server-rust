use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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
    pub event_bus: Option<crate::event_bus::EventBus>,
}

impl PluginContext {
    pub fn new(api_base_url: &str, data_dir: PathBuf) -> Self {
        Self {
            api_base_url: api_base_url.to_string(),
            data_dir,
            event_bus: None,
        }
    }

    pub fn with_event_bus(mut self, bus: crate::event_bus::EventBus) -> Self {
        self.event_bus = Some(bus);
        self
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
    async fn on_event(&mut self, _event: &crate::event_bus::TuneEvent) {}

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
    plugins: Vec<Box<dyn TunePlugin>>,
    data_root: PathBuf,
    event_bus: Option<crate::event_bus::EventBus>,
}

impl PluginLoader {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            plugins: Vec::new(),
            data_root,
            event_bus: None,
        }
    }

    pub fn with_event_bus(mut self, bus: crate::event_bus::EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn register(&mut self, plugin: Box<dyn TunePlugin>) {
        self.plugins.push(plugin);
    }

    pub async fn setup_all(&mut self, api_base_url: &str) -> Vec<String> {
        let mut loaded = Vec::new();
        std::fs::create_dir_all(&self.data_root).ok();

        for plugin in &mut self.plugins {
            let name = plugin.name().to_string();
            let data_dir = self.data_root.join(&name);
            std::fs::create_dir_all(&data_dir).ok();

            let ctx = PluginContext::new(api_base_url, data_dir);

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

    pub async fn teardown_all(&mut self) {
        for plugin in self.plugins.iter_mut().rev() {
            let name = plugin.name().to_string();
            if let Err(e) = plugin.teardown().await {
                warn!(plugin_name = %name, error = %e, "plugin_teardown_failed");
            }
        }
        self.plugins.clear();
    }

    pub fn loaded_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
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

    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn loader_setup_and_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(TestPlugin::new()));

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
        assert_eq!(loader.plugin_count(), 1);

        let info = loader.loaded_plugins();
        assert_eq!(info[0].name, "test-plugin");
        assert_eq!(info[0].version, "0.1.0");

        loader.teardown_all().await;
        assert_eq!(loader.plugin_count(), 0);
    }

    #[tokio::test]
    async fn failing_plugin_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailingPlugin));
        loader.register(Box::new(TestPlugin::new()));

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
    }

    #[test]
    fn plugin_context() {
        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp/test"));
        assert_eq!(ctx.api_base_url, "http://localhost");
        assert!(ctx.event_bus.is_none());
    }

    #[test]
    fn empty_loader() {
        let loader = PluginLoader::new(PathBuf::from("/tmp"));
        assert_eq!(loader.plugin_count(), 0);
        assert!(loader.loaded_plugins().is_empty());
    }
}
