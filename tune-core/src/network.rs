use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkShare {
    pub host: String,
    pub share_name: String,
    pub share_type: ShareType,
    pub mount_point: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareType {
    Smb,
    Nfs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredShare {
    pub host: String,
    pub name: String,
    pub share_type: ShareType,
}

pub struct MountManager {
    mounts: Mutex<HashMap<String, NetworkShare>>,
    credentials: Mutex<HashMap<String, ShareCredential>>,
}

#[derive(Debug, Clone)]
struct ShareCredential {
    username: String,
    password: String,
}

impl Default for MountManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MountManager {
    pub fn new() -> Self {
        Self {
            mounts: Mutex::new(HashMap::new()),
            credentials: Mutex::new(HashMap::new()),
        }
    }

    pub async fn store_credentials(&self, host: &str, username: &str, password: &str) {
        self.credentials.lock().await.insert(
            host.to_string(),
            ShareCredential {
                username: username.to_string(),
                password: password.to_string(),
            },
        );
    }

    pub async fn mount_smb(
        &self,
        host: &str,
        share_name: &str,
        mount_point: &str,
    ) -> Result<(), String> {
        let cred = self.credentials.lock().await.get(host).cloned();

        let mut args: Vec<String> = if cfg!(target_os = "macos") {
            let unc = format!("//{}/{}", host, share_name);
            vec![
                "-t".into(),
                "smbfs".into(),
                unc,
                mount_point.into(),
            ]
        } else {
            let unc = format!("//{}/{}", host, share_name);
            let mut a = vec![
                "-t".into(),
                "cifs".into(),
                unc,
                mount_point.into(),
                "-o".into(),
            ];
            let opts = if let Some(ref c) = cred {
                format!("username={},password={}", c.username, c.password)
            } else {
                "guest".into()
            };
            a.push(opts);
            a
        };

        tokio::fs::create_dir_all(mount_point)
            .await
            .map_err(|e| format!("mkdir: {e}"))?;

        let output = tokio::process::Command::new("mount")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("mount: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("mount failed: {stderr}"));
        }

        let share = NetworkShare {
            host: host.into(),
            share_name: share_name.into(),
            share_type: ShareType::Smb,
            mount_point: Some(mount_point.into()),
            username: cred.as_ref().map(|c| c.username.clone()),
        };

        let key = format!("{}:{}", host, share_name);
        self.mounts.lock().await.insert(key, share);
        info!(host, share_name, mount_point, "smb_mounted");
        Ok(())
    }

    pub async fn mount_nfs(
        &self,
        host: &str,
        export_path: &str,
        mount_point: &str,
    ) -> Result<(), String> {
        tokio::fs::create_dir_all(mount_point)
            .await
            .map_err(|e| format!("mkdir: {e}"))?;

        let source = format!("{host}:{export_path}");
        let output = tokio::process::Command::new("mount")
            .args(["-t", "nfs", &source, mount_point])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("mount: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("mount failed: {stderr}"));
        }

        let share = NetworkShare {
            host: host.into(),
            share_name: export_path.into(),
            share_type: ShareType::Nfs,
            mount_point: Some(mount_point.into()),
            username: None,
        };

        let key = format!("{}:{}", host, export_path);
        self.mounts.lock().await.insert(key, share);
        info!(host, export_path, mount_point, "nfs_mounted");
        Ok(())
    }

    pub async fn unmount(&self, mount_point: &str) -> Result<(), String> {
        let cmd = if cfg!(target_os = "macos") {
            "umount"
        } else {
            "umount"
        };

        let output = tokio::process::Command::new(cmd)
            .arg(mount_point)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("umount: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("umount failed: {stderr}"));
        }

        let mut mounts = self.mounts.lock().await;
        mounts.retain(|_, v| v.mount_point.as_deref() != Some(mount_point));

        info!(mount_point, "share_unmounted");
        Ok(())
    }

    pub async fn list_mounts(&self) -> Vec<NetworkShare> {
        self.mounts.lock().await.values().cloned().collect()
    }

    pub async fn is_mounted(&self, host: &str, share_name: &str) -> bool {
        let key = format!("{host}:{share_name}");
        self.mounts.lock().await.contains_key(&key)
    }
}

pub async fn discover_smb_shares(subnet: Option<&str>) -> Result<Vec<DiscoveredShare>, String> {
    let tool = if cfg!(target_os = "macos") {
        "smbutil"
    } else {
        "smbclient"
    };

    let args: Vec<&str> = if cfg!(target_os = "macos") {
        vec!["lookup", "-a"]
    } else {
        vec!["-L", subnet.unwrap_or(""), "-N"]
    };

    let output = tokio::process::Command::new(tool)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("smb discovery: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut shares = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("---") {
            continue;
        }
        if let Some((name, rest)) = line.split_once(char::is_whitespace) {
            if rest.to_lowercase().contains("disk") {
                shares.push(DiscoveredShare {
                    host: subnet.unwrap_or("unknown").into(),
                    name: name.trim().to_string(),
                    share_type: ShareType::Smb,
                });
            }
        }
    }

    Ok(shares)
}

pub async fn discover_nfs_exports(host: &str) -> Result<Vec<DiscoveredShare>, String> {
    let output = tokio::process::Command::new("showmount")
        .args(["-e", host])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("nfs discovery: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut shares = Vec::new();

    for line in stdout.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((path, _)) = line.split_once(char::is_whitespace) {
            shares.push(DiscoveredShare {
                host: host.into(),
                name: path.trim().to_string(),
                share_type: ShareType::Nfs,
            });
        }
    }

    Ok(shares)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mount_manager_empty() {
        let mgr = MountManager::new();
        assert!(mgr.list_mounts().await.is_empty());
        assert!(!mgr.is_mounted("host", "share").await);
    }

    #[tokio::test]
    async fn store_credentials() {
        let mgr = MountManager::new();
        mgr.store_credentials("192.168.1.1", "user", "pass").await;
        let creds = mgr.credentials.lock().await;
        assert!(creds.contains_key("192.168.1.1"));
    }

    #[test]
    fn share_type_serialize() {
        let share = NetworkShare {
            host: "nas.local".into(),
            share_name: "music".into(),
            share_type: ShareType::Smb,
            mount_point: Some("/mnt/music".into()),
            username: None,
        };
        let json = serde_json::to_value(&share).unwrap();
        assert_eq!(json["share_type"], "smb");
        assert_eq!(json["host"], "nas.local");
    }
}
