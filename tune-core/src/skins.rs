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
        if self.skins_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&self.skins_dir)
        {
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
        if !id_de_theme_sur(skin_id) {
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
                let name = file.name().ok()?.into_owned();
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
        // L'id nomme le dossier du thème ET un préfixe d'URL (`/{id}`).
        if !id_de_theme_sur(&manifest.id) {
            return Err(format!("unsafe skin id in skin.json: {:?}", manifest.id));
        }

        // Plan d'extraction, établi AVANT de toucher au disque : chaque chemin
        // reste sous le dossier du thème. Seuls les dossiers et les fichiers
        // ordinaires sont repris — ni lien symbolique, ni droits de l'archive.
        let mut plan: Vec<(usize, PathBuf, bool)> = Vec::new();
        for i in 0..archive.len() {
            let file = archive.by_index(i).map_err(|e| e.to_string())?;
            let name = file.name().map_err(|e| e.to_string())?.into_owned();

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
            if !file.is_dir() && !file.is_file() {
                continue;
            }
            let relative_path = Path::new(&relative);
            if !relative_path.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            }) {
                return Err(format!("unsafe path in archive: {name:?}"));
            }
            plan.push((i, relative_path.to_path_buf(), file.is_dir()));
        }

        let target_dir = self.skins_dir.join(&manifest.id);
        if target_dir.exists() {
            std::fs::remove_dir_all(&target_dir).map_err(|e| format!("remove old skin: {e}"))?;
        }
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("create skin dir: {e}"))?;

        // Extract files
        for (i, relative, is_dir) in plan {
            let out_path = target_dir.join(&relative);
            if is_dir {
                std::fs::create_dir_all(&out_path).ok();
            } else {
                let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let mut out_file = std::fs::File::create(&out_path)
                    .map_err(|e| format!("create {}: {e}", relative.display()))?;
                std::io::copy(&mut file, &mut out_file)
                    .map_err(|e| format!("write {}: {e}", relative.display()))?;
            }
        }

        info!(skin_id = %manifest.id, skin_name = %manifest.name, "skin_installed");
        Ok(manifest)
    }

    pub fn uninstall(&self, skin_id: &str) -> Result<(), String> {
        if skin_id == "default" {
            return Err("cannot uninstall the default skin".into());
        }
        if !id_de_theme_sur(skin_id) {
            return Err(format!("skin '{skin_id}' not found"));
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

/// Un id de thème devient un nom de dossier sous `skins_dir` et un préfixe
/// d'URL : une seule composante, sans séparateur ni `.`/`..`.
fn id_de_theme_sur(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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

#[cfg(test)]
mod tests {
    use super::SkinManager;
    use std::io::Write;

    fn manifeste(id: &str) -> Vec<u8> {
        serde_json::json!({ "id": id, "name": "Essai", "version": "1.0.0" })
            .to_string()
            .into_bytes()
    }

    /// Une archive de thème : (nom, contenu, droits unix éventuels), plus des
    /// liens symboliques (nom, cible).
    fn archive(fichiers: &[(&str, &[u8], Option<u32>)], liens: &[(&str, &str)]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (nom, contenu, droits) in fichiers {
            let mut o = zip::write::SimpleFileOptions::default();
            if let Some(d) = droits {
                o = o.unix_permissions(*d);
            }
            z.start_file(*nom, o).unwrap();
            z.write_all(contenu).unwrap();
        }
        for (nom, cible) in liens {
            z.add_symlink(*nom, *cible, zip::write::SimpleFileOptions::default())
                .unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    fn gestionnaire(racine: &std::path::Path) -> SkinManager {
        let skins = racine.join("skins");
        std::fs::create_dir_all(&skins).unwrap();
        SkinManager::new(skins, racine.join("web"))
    }

    #[test]
    fn un_chemin_qui_remonte_hors_du_theme_est_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let m = gestionnaire(tmp.path());
        let data = archive(
            &[
                ("theme/skin.json", &manifeste("essai"), None),
                ("theme/../../evade.txt", b"x", None),
                ("theme/../voisin/evade.txt", b"x", None),
            ],
            &[],
        );
        let err = m.install_from_zip(&data).unwrap_err();
        assert!(err.contains("unsafe path"), "{err}");
        assert!(!tmp.path().join("evade.txt").exists());
        assert!(!tmp.path().join("skins/voisin").exists());
        assert!(!tmp.path().join("skins/essai").exists());
    }

    #[test]
    fn un_id_de_theme_qui_traverse_est_refuse() {
        let tmp = tempfile::tempdir().unwrap();
        let m = gestionnaire(tmp.path());
        let temoin = tmp.path().join("victime");
        std::fs::create_dir_all(&temoin).unwrap();
        std::fs::write(temoin.join("garde.txt"), b"garde").unwrap();
        let data = archive(
            &[
                ("skin.json", &manifeste("../victime"), None),
                ("index.html", b"<p>", None),
            ],
            &[],
        );
        let err = m.install_from_zip(&data).unwrap_err();
        assert!(err.contains("unsafe skin id"), "{err}");
        assert!(
            temoin.join("garde.txt").exists(),
            "le dossier voisin a été vidé"
        );
        assert!(!temoin.join("index.html").exists());
        // Et la désinstallation ne sort pas davantage du dossier des thèmes.
        assert!(m.uninstall("../victime").is_err());
        assert!(temoin.join("garde.txt").exists());
        assert!(m.skin_dir("../victime").is_none());
    }

    #[test]
    fn ni_lien_symbolique_ni_bit_suid_ne_sont_restaures() {
        let tmp = tempfile::tempdir().unwrap();
        let m = gestionnaire(tmp.path());
        let data = archive(
            &[
                ("skin.json", &manifeste("essai"), None),
                ("index.html", b"<p>", None),
                ("outil.sh", b"#!/bin/sh\n", Some(0o6755)),
            ],
            &[("sortie", "../../.."), ("absolu", "/etc")],
        );
        let manifest = m.install_from_zip(&data).unwrap();
        assert_eq!(manifest.id, "essai");
        let dossier = tmp.path().join("skins/essai");
        assert!(dossier.join("index.html").is_file());
        for lien in ["sortie", "absolu"] {
            let meta = std::fs::symlink_metadata(dossier.join(lien));
            assert!(
                meta.map(|m| !m.file_type().is_symlink()).unwrap_or(true),
                "lien symbolique {lien} restauré"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dossier.join("outil.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o7000, 0, "bits spéciaux restaurés : {mode:o}");
        }
    }
}
