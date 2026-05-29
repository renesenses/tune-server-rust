use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareProtocol {
    Smb,
    Nfs,
}

impl ShareProtocol {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Smb => "smb",
            Self::Nfs => "nfs",
        }
    }

    fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "nfs" => Self::Nfs,
            _ => Self::Smb,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub id: i64,
    pub host: String,
    pub share_name: String,
    pub protocol: ShareProtocol,
    pub mount_path: String,
    pub username: Option<String>,
    pub auto_mount: bool,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MountRequest {
    pub host: String,
    pub share_name: String,
    pub protocol: ShareProtocol,
    pub username: Option<String>,
    pub password: Option<String>,
    pub auto_mount: bool,
}

pub struct MountManager {
    db: SqliteDb,
    mount_base: PathBuf,
}

impl MountManager {
    pub fn new(db: SqliteDb, mount_base_dir: &str) -> Self {
        let expanded = if mount_base_dir.starts_with('~') {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            mount_base_dir.replacen('~', &home, 1)
        } else {
            mount_base_dir.to_string()
        };
        Self {
            db,
            mount_base: PathBuf::from(expanded),
        }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS network_mounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                host TEXT NOT NULL,
                share_name TEXT NOT NULL,
                protocol TEXT NOT NULL DEFAULT 'smb',
                mount_path TEXT NOT NULL,
                username TEXT,
                password TEXT,
                auto_mount INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'unmounted',
                created_at TEXT DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(host, share_name)
            );",
        )
    }

    pub fn list_mounts(&self) -> Result<Vec<MountInfo>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, host, share_name, protocol, mount_path, \
                 username, auto_mount, status FROM network_mounts ORDER BY host, share_name",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map([], |row| {
                let proto_str: String = row.get(3)?;
                Ok(MountInfo {
                    id: row.get(0)?,
                    host: row.get(1)?,
                    share_name: row.get(2)?,
                    protocol: ShareProtocol::from_str(&proto_str),
                    mount_path: row.get(4)?,
                    username: row.get(5)?,
                    auto_mount: row.get::<_, i64>(6)? != 0,
                    status: row.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn add_mount(&self, req: &MountRequest) -> Result<MountInfo, String> {
        let safe_name = sanitize_mount_name(&req.host, &req.share_name);
        let mount_path = self.mount_base.join(&safe_name);

        std::fs::create_dir_all(&mount_path).map_err(|e| e.to_string())?;

        let mount_path_str = mount_path.to_str().unwrap_or("").to_string();

        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO network_mounts \
             (host, share_name, protocol, mount_path, username, password, auto_mount, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'unmounted')",
            rusqlite::params![
                req.host,
                req.share_name,
                req.protocol.as_str(),
                mount_path_str,
                req.username,
                req.password,
                if req.auto_mount { 1 } else { 0 },
            ],
        )
        .map_err(|e| e.to_string())?;

        let id = conn.last_insert_rowid();
        info!(id, host = %req.host, share = %req.share_name, "mount_added");

        Ok(MountInfo {
            id,
            host: req.host.clone(),
            share_name: req.share_name.clone(),
            protocol: req.protocol,
            mount_path: mount_path_str,
            username: req.username.clone(),
            auto_mount: req.auto_mount,
            status: "unmounted".into(),
        })
    }

    pub fn remove_mount(&self, id: i64) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute("DELETE FROM network_mounts WHERE id = ?1", [id])
            .map_err(|e| e.to_string())?;
        info!(id, "mount_removed");
        Ok(())
    }

    pub fn update_status(&self, id: i64, status: &str) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE network_mounts SET status = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            rusqlite::params![status, id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn mount_smb(&self, mount: &MountInfo, password: Option<&str>) -> Result<(), String> {
        let mount_path = Path::new(&mount.mount_path);
        std::fs::create_dir_all(mount_path).map_err(|e| e.to_string())?;

        let result = if cfg!(target_os = "macos") {
            mount_smb_macos(&mount.host, &mount.share_name, mount_path, mount.username.as_deref(), password).await
        } else {
            mount_smb_linux(&mount.host, &mount.share_name, mount_path, mount.username.as_deref(), password).await
        };

        match result {
            Ok(()) => {
                self.update_status(mount.id, "mounted")?;
                info!(id = mount.id, host = %mount.host, share = %mount.share_name, "mount_success");
                Ok(())
            }
            Err(e) => {
                self.update_status(mount.id, "error")?;
                warn!(id = mount.id, error = %e, "mount_failed");
                Err(e)
            }
        }
    }

    pub async fn unmount(&self, mount: &MountInfo) -> Result<(), String> {
        let output = tokio::process::Command::new("umount")
            .arg(&mount.mount_path)
            .output()
            .await
            .map_err(|e| e.to_string())?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(path = %mount.mount_path, error = %stderr, "unmount_failed");
        }

        self.update_status(mount.id, "unmounted")?;
        Ok(())
    }
}

fn sanitize_mount_name(host: &str, share: &str) -> String {
    let raw = format!("{host}_{share}");
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn mount_smb_macos(
    host: &str,
    share: &str,
    mount_point: &Path,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), String> {
    let url = match (username, password) {
        (Some(u), Some(p)) => format!("smb://{u}:{p}@{host}/{share}"),
        (Some(u), None) => format!("smb://{u}@{host}/{share}"),
        _ => format!("smb://{host}/{share}"),
    };

    let output = tokio::process::Command::new("mount_smbfs")
        .arg(&url)
        .arg(mount_point)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("mount_smbfs failed: {stderr}"))
    }
}

async fn mount_smb_linux(
    host: &str,
    share: &str,
    mount_point: &Path,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), String> {
    let unc = format!("//{host}/{share}");
    let cred = match (username, password) {
        (Some(u), Some(p)) => format!("username={u},password={p}"),
        (Some(u), None) => format!("username={u}"),
        _ => "guest".into(),
    };

    let output = tokio::process::Command::new("mount")
        .args(["-t", "cifs"])
        .arg(&unc)
        .arg(mount_point)
        .args(["-o", &cred])
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("mount.cifs failed: {stderr}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> MountManager {
        let db = SqliteDb::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mgr = MountManager::new(db, dir.path().to_str().unwrap());
        mgr.setup_table().unwrap();
        mgr
    }

    #[test]
    fn sanitize_name() {
        assert_eq!(sanitize_mount_name("192.168.1.1", "Music"), "192.168.1.1_Music");
        assert_eq!(sanitize_mount_name("server/bad", "sh@re"), "server_bad_sh_re");
    }

    #[test]
    fn add_and_list_mount() {
        let mgr = setup();
        let req = MountRequest {
            host: "192.168.1.100".into(),
            share_name: "Music".into(),
            protocol: ShareProtocol::Smb,
            username: Some("user".into()),
            password: None,
            auto_mount: true,
        };
        let mount = mgr.add_mount(&req).unwrap();
        assert_eq!(mount.host, "192.168.1.100");
        assert!(mount.auto_mount);

        let list = mgr.list_mounts().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].share_name, "Music");
    }

    #[test]
    fn remove_mount() {
        let mgr = setup();
        let req = MountRequest {
            host: "10.0.0.1".into(),
            share_name: "Share".into(),
            protocol: ShareProtocol::Nfs,
            username: None,
            password: None,
            auto_mount: false,
        };
        let mount = mgr.add_mount(&req).unwrap();
        mgr.remove_mount(mount.id).unwrap();
        assert!(mgr.list_mounts().unwrap().is_empty());
    }

    #[test]
    fn update_status() {
        let mgr = setup();
        let req = MountRequest {
            host: "host".into(),
            share_name: "share".into(),
            protocol: ShareProtocol::Smb,
            username: None,
            password: None,
            auto_mount: false,
        };
        let mount = mgr.add_mount(&req).unwrap();
        mgr.update_status(mount.id, "mounted").unwrap();

        let list = mgr.list_mounts().unwrap();
        assert_eq!(list[0].status, "mounted");
    }

    #[test]
    fn protocol_roundtrip() {
        assert_eq!(ShareProtocol::from_str("smb"), ShareProtocol::Smb);
        assert_eq!(ShareProtocol::from_str("nfs"), ShareProtocol::Nfs);
        assert_eq!(ShareProtocol::Smb.as_str(), "smb");
        assert_eq!(ShareProtocol::Nfs.as_str(), "nfs");
    }
}
