use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkinManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub framework: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub premium: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub min_server_version: Option<String>,
}

fn default_api_version() -> String {
    "1".into()
}

fn default_entry() -> String {
    "index.html".into()
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledSkin {
    pub manifest: SkinManifest,
    pub path: PathBuf,
    pub size_bytes: u64,
}

pub struct SkinManager {
    skins_dir: PathBuf,
    web_dir: PathBuf,
}

impl SkinManager {
    pub fn new(skins_dir: PathBuf, web_dir: PathBuf) -> Self {
        Self { skins_dir, web_dir }
    }

    pub fn skins_dir(&self) -> &Path {
        &self.skins_dir
    }

    pub fn web_dir(&self) -> &Path {
        &self.web_dir
    }

    pub fn ensure_dirs(&self) {
        if !self.skins_dir.exists() {
            std::fs::create_dir_all(&self.skins_dir).ok();
        }
    }

    pub fn list(&self) -> Vec<InstalledSkin> {
        let mut skins = Vec::new();

        // Built-in default skin from web_dir
        if self.web_dir.exists() {
            let manifest_path = self.web_dir.join("skin.json");
            let manifest = if manifest_path.exists() {
                std::fs::read_to_string(&manifest_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<SkinManifest>(&s).ok())
            } else {
                None
            }
            .unwrap_or_else(|| SkinManifest {
                id: "default".into(),
                name: "Tune".into(),
                version: "1.0.0".into(),
                author: "Mozaik Labs".into(),
                description: "Interface standard Tune".into(),
                framework: "svelte".into(),
                api_version: "1".into(),
                preview: None,
                entry: "index.html".into(),
                premium: false,
                tags: vec!["official".into()],
                min_server_version: None,
            });

            skins.push(InstalledSkin {
                manifest,
                path: self.web_dir.clone(),
                size_bytes: dir_size(&self.web_dir),
            });
        }

        // Scan skins/ directory
        if self.skins_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&self.skins_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let manifest_path = path.join("skin.json");
                    if !manifest_path.exists() {
                        continue;
                    }
                    match std::fs::read_to_string(&manifest_path) {
                        Ok(content) => match serde_json::from_str::<SkinManifest>(&content) {
                            Ok(manifest) => {
                                info!(skin_id = %manifest.id, skin_name = %manifest.name, "skin_discovered");
                                skins.push(InstalledSkin {
                                    manifest,
                                    path: path.clone(),
                                    size_bytes: dir_size(&path),
                                });
                            }
                            Err(e) => {
                                warn!(path = %manifest_path.display(), error = %e, "skin_manifest_parse_error");
                            }
                        },
                        Err(e) => {
                            warn!(path = %manifest_path.display(), error = %e, "skin_manifest_read_error");
                        }
                    }
                }
            }
        }

        skins
    }

    pub fn get(&self, skin_id: &str) -> Option<InstalledSkin> {
        self.list().into_iter().find(|s| s.manifest.id == skin_id)
    }

    pub fn skin_dir(&self, skin_id: &str) -> Option<PathBuf> {
        if skin_id == "default" {
            if self.web_dir.exists() {
                return Some(self.web_dir.clone());
            }
            return None;
        }
        let dir = self.skins_dir.join(skin_id);
        if dir.exists() && dir.join("skin.json").exists() {
            Some(dir)
        } else {
            None
        }
    }

    pub fn install_from_zip(&self, zip_data: &[u8]) -> Result<SkinManifest, String> {
        let cursor = std::io::Cursor::new(zip_data);
        let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("invalid zip: {e}"))?;

        // Find skin.json in the archive (may be at root or in a subdirectory)
        let manifest_path = (0..archive.len())
            .filter_map(|i| {
                let file = archive.by_index(i).ok()?;
                let name = file.name().to_string();
                if name.ends_with("skin.json") && name.matches('/').count() <= 1 {
                    Some(name)
                } else {
                    None
                }
            })
            .next()
            .ok_or("skin.json not found in archive")?;

        let prefix = if manifest_path == "skin.json" {
            String::new()
        } else {
            manifest_path
                .strip_suffix("skin.json")
                .unwrap_or("")
                .to_string()
        };

        // Parse manifest
        let manifest: SkinManifest = {
            let file = archive
                .by_name(&manifest_path)
                .map_err(|e| format!("read skin.json: {e}"))?;
            serde_json::from_reader(file).map_err(|e| format!("parse skin.json: {e}"))?
        };

        let target_dir = self.skins_dir.join(&manifest.id);
        if target_dir.exists() {
            std::fs::remove_dir_all(&target_dir).map_err(|e| format!("remove old skin: {e}"))?;
        }
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("create skin dir: {e}"))?;

        // Extract files
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
            let name = file.name().to_string();

            let relative = if prefix.is_empty() {
                name.clone()
            } else if let Some(stripped) = name.strip_prefix(&prefix) {
                stripped.to_string()
            } else {
                continue;
            };

            if relative.is_empty() {
                continue;
            }

            let out_path = target_dir.join(&relative);
            if file.is_dir() {
                std::fs::create_dir_all(&out_path).ok();
            } else {
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let mut out_file = std::fs::File::create(&out_path)
                    .map_err(|e| format!("create {relative}: {e}"))?;
                std::io::copy(&mut file, &mut out_file)
                    .map_err(|e| format!("write {relative}: {e}"))?;
            }
        }

        info!(skin_id = %manifest.id, skin_name = %manifest.name, "skin_installed");
        Ok(manifest)
    }

    pub fn uninstall(&self, skin_id: &str) -> Result<(), String> {
        if skin_id == "default" {
            return Err("cannot uninstall the default skin".into());
        }
        let dir = self.skins_dir.join(skin_id);
        if !dir.exists() {
            return Err(format!("skin '{skin_id}' not found"));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("remove skin: {e}"))?;
        info!(skin_id, "skin_uninstalled");
        Ok(())
    }

    pub fn mountable_skins(&self) -> HashMap<String, PathBuf> {
        let mut mounts = HashMap::new();
        for skin in self.list() {
            if skin.manifest.id != "default" {
                mounts.insert(skin.manifest.id.clone(), skin.path);
            }
        }
        mounts
    }
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}
