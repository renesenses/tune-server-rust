use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub entry_point: String,
    pub permissions: Vec<String>,
    pub min_server_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginState {
    Installed,
    Active,
    Disabled,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub manifest: PluginManifest,
    pub state: PluginState,
    pub path: String,
    pub error: Option<String>,
}

pub struct PluginManager {
    plugins_dir: PathBuf,
    plugins: Mutex<HashMap<String, PluginInfo>>,
}

impl PluginManager {
    pub fn new(plugins_dir: PathBuf) -> Self {
        Self {
            plugins_dir,
            plugins: Mutex::new(HashMap::new()),
        }
    }

    pub async fn scan(&self) -> Result<Vec<PluginInfo>, String> {
        let mut found = Vec::new();

        if !self.plugins_dir.exists() {
            return Ok(found);
        }

        let entries =
            std::fs::read_dir(&self.plugins_dir).map_err(|e| format!("read plugins dir: {e}"))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                continue;
            }

            match load_manifest(&manifest_path) {
                Ok(manifest) => {
                    let info = PluginInfo {
                        manifest: manifest.clone(),
                        state: PluginState::Installed,
                        path: path.to_string_lossy().to_string(),
                        error: None,
                    };
                    found.push(info.clone());
                    self.plugins.lock().await.insert(manifest.id, info);
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "plugin_manifest_error");
                }
            }
        }

        info!(count = found.len(), "plugins_scanned");
        Ok(found)
    }

    pub async fn enable(&self, plugin_id: &str) -> Result<(), String> {
        let mut plugins = self.plugins.lock().await;
        let plugin = plugins
            .get_mut(plugin_id)
            .ok_or_else(|| format!("plugin not found: {plugin_id}"))?;

        if plugin.state == PluginState::Active {
            return Ok(());
        }

        let entry = Path::new(&plugin.path).join(&plugin.manifest.entry_point);
        if !entry.exists() {
            plugin.state = PluginState::Error;
            plugin.error = Some("entry point not found".into());
            return Err("entry point not found".into());
        }

        plugin.state = PluginState::Active;
        plugin.error = None;
        info!(id = plugin_id, "plugin_enabled");
        Ok(())
    }

    pub async fn disable(&self, plugin_id: &str) -> Result<(), String> {
        let mut plugins = self.plugins.lock().await;
        let plugin = plugins
            .get_mut(plugin_id)
            .ok_or_else(|| format!("plugin not found: {plugin_id}"))?;

        plugin.state = PluginState::Disabled;
        info!(id = plugin_id, "plugin_disabled");
        Ok(())
    }

    pub async fn uninstall(&self, plugin_id: &str) -> Result<(), String> {
        let mut plugins = self.plugins.lock().await;
        let plugin = plugins
            .remove(plugin_id)
            .ok_or_else(|| format!("plugin not found: {plugin_id}"))?;

        let path = Path::new(&plugin.path);
        if path.exists() {
            std::fs::remove_dir_all(path).map_err(|e| format!("remove plugin: {e}"))?;
        }

        info!(id = plugin_id, "plugin_uninstalled");
        Ok(())
    }

    pub async fn list(&self) -> Vec<PluginInfo> {
        self.plugins.lock().await.values().cloned().collect()
    }

    pub async fn get(&self, plugin_id: &str) -> Option<PluginInfo> {
        self.plugins.lock().await.get(plugin_id).cloned()
    }

    pub async fn active_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .lock()
            .await
            .values()
            .filter(|p| p.state == PluginState::Active)
            .cloned()
            .collect()
    }

    pub async fn install_from_archive(&self, archive_data: &[u8]) -> Result<PluginInfo, String> {
        let temp_dir = std::env::temp_dir().join(format!("tune_plugin_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).map_err(|e| format!("create temp dir: {e}"))?;

        let archive_path = temp_dir.join("plugin.tar.gz");
        std::fs::write(&archive_path, archive_data).map_err(|e| format!("write archive: {e}"))?;

        let output = std::process::Command::new("tar")
            .args([
                "xzf",
                &archive_path.to_string_lossy(),
                "-C",
                &temp_dir.to_string_lossy(),
            ])
            .output()
            .map_err(|e| format!("extract: {e}"))?;

        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err("archive extraction failed".into());
        }

        let manifest_path = temp_dir.join("manifest.json");
        let manifest = load_manifest(&manifest_path)?;

        let target_dir = self.plugins_dir.join(&manifest.id);
        if target_dir.exists() {
            std::fs::remove_dir_all(&target_dir).map_err(|e| format!("remove old: {e}"))?;
        }

        std::fs::rename(&temp_dir, &target_dir).map_err(|e| format!("install: {e}"))?;

        let info = PluginInfo {
            manifest: manifest.clone(),
            state: PluginState::Installed,
            path: target_dir.to_string_lossy().to_string(),
            error: None,
        };

        self.plugins.lock().await.insert(manifest.id, info.clone());

        info!(id = %info.manifest.id, name = %info.manifest.name, "plugin_installed");
        Ok(info)
    }
}

fn load_manifest(path: &Path) -> Result<PluginManifest, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read manifest: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("parse manifest: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_manifest() {
        let json = r#"{
            "id": "my-plugin",
            "name": "My Plugin",
            "version": "1.0.0",
            "description": "Test plugin",
            "author": "Test",
            "entry_point": "main.wasm",
            "permissions": ["playback", "library"],
            "min_server_version": "1.0.0"
        }"#;

        let manifest: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.id, "my-plugin");
        assert_eq!(manifest.permissions.len(), 2);
    }

    #[tokio::test]
    async fn scan_empty_dir() {
        let dir = std::env::temp_dir().join("tune_plugins_test_empty");
        fs::create_dir_all(&dir).ok();

        let mgr = PluginManager::new(dir.clone());
        let plugins = mgr.scan().await.unwrap();
        assert!(plugins.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn scan_with_plugin() {
        let dir = std::env::temp_dir().join("tune_plugins_test_scan");
        let plugin_dir = dir.join("test-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = r#"{
            "id": "test-plugin",
            "name": "Test",
            "version": "1.0.0",
            "description": "Test",
            "author": "Test",
            "entry_point": "main.wasm",
            "permissions": []
        }"#;
        fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();

        let mgr = PluginManager::new(dir.clone());
        let plugins = mgr.scan().await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].manifest.id, "test-plugin");

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn enable_disable_plugin() {
        let dir = std::env::temp_dir().join("tune_plugins_test_enable");
        let plugin_dir = dir.join("my-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = r#"{
            "id": "my-plugin",
            "name": "My Plugin",
            "version": "1.0.0",
            "description": "Test",
            "author": "Test",
            "entry_point": "main.wasm",
            "permissions": []
        }"#;
        fs::write(plugin_dir.join("manifest.json"), manifest).unwrap();
        fs::write(plugin_dir.join("main.wasm"), b"fake wasm").unwrap();

        let mgr = PluginManager::new(dir.clone());
        mgr.scan().await.unwrap();

        mgr.enable("my-plugin").await.unwrap();
        let info = mgr.get("my-plugin").await.unwrap();
        assert_eq!(info.state, PluginState::Active);

        mgr.disable("my-plugin").await.unwrap();
        let info = mgr.get("my-plugin").await.unwrap();
        assert_eq!(info.state, PluginState::Disabled);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn list_active_only() {
        let dir = std::env::temp_dir().join("tune_plugins_test_active");
        fs::create_dir_all(&dir).ok();

        let mgr = PluginManager::new(dir.clone());

        let mut plugins = mgr.plugins.lock().await;
        plugins.insert(
            "a".into(),
            PluginInfo {
                manifest: PluginManifest {
                    id: "a".into(),
                    name: "A".into(),
                    version: "1.0.0".into(),
                    description: String::new(),
                    author: String::new(),
                    entry_point: String::new(),
                    permissions: vec![],
                    min_server_version: None,
                },
                state: PluginState::Active,
                path: String::new(),
                error: None,
            },
        );
        plugins.insert(
            "b".into(),
            PluginInfo {
                manifest: PluginManifest {
                    id: "b".into(),
                    name: "B".into(),
                    version: "1.0.0".into(),
                    description: String::new(),
                    author: String::new(),
                    entry_point: String::new(),
                    permissions: vec![],
                    min_server_version: None,
                },
                state: PluginState::Disabled,
                path: String::new(),
                error: None,
            },
        );
        drop(plugins);

        let active = mgr.active_plugins().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].manifest.id, "a");

        fs::remove_dir_all(&dir).ok();
    }
}
