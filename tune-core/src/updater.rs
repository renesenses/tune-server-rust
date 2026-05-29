use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/mozaiklabs/tune-server/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 6 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub name: String,
    pub body: String,
    pub published_at: String,
    pub html_url: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub content_type: String,
}

impl ReleaseInfo {
    pub fn asset_for_platform(&self) -> Option<&ReleaseAsset> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        self.assets.iter().find(|a| {
            let name = a.name.to_lowercase();
            let os_match = match os {
                "macos" => {
                    name.contains("macos") || name.contains("darwin") || name.contains(".dmg")
                }
                "linux" => name.contains("linux") || name.contains(".tar.gz"),
                "windows" => name.contains("windows") || name.contains(".exe"),
                _ => false,
            };
            let arch_match = match arch {
                "aarch64" => name.contains("aarch64") || name.contains("arm64"),
                "x86_64" => name.contains("x86_64") || name.contains("amd64"),
                _ => true,
            };
            os_match && arch_match
        })
    }
}

pub struct UpdateChecker {
    client: reqwest::Client,
    current_version: String,
}

impl UpdateChecker {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("tune-server")
                .build()
                .unwrap(),
            current_version: crate::version().to_string(),
        }
    }

    pub async fn check(&self) -> Result<Option<ReleaseInfo>, String> {
        let resp = self
            .client
            .get(GITHUB_RELEASES_URL)
            .send()
            .await
            .map_err(|e| format!("update check: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("github api: {}", resp.status()));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("update parse: {e}"))?;

        let tag = data["tag_name"].as_str().unwrap_or("").to_string();
        let version = tag.trim_start_matches('v').to_string();

        if version.is_empty() || !is_newer(&version, &self.current_version) {
            return Ok(None);
        }

        let assets: Vec<ReleaseAsset> = data["assets"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|a| ReleaseAsset {
                name: a["name"].as_str().unwrap_or("").to_string(),
                browser_download_url: a["browser_download_url"].as_str().unwrap_or("").to_string(),
                size: a["size"].as_u64().unwrap_or(0),
                content_type: a["content_type"].as_str().unwrap_or("").to_string(),
            })
            .collect();

        Ok(Some(ReleaseInfo {
            tag_name: tag,
            version,
            name: data["name"].as_str().unwrap_or("").to_string(),
            body: data["body"].as_str().unwrap_or("").to_string(),
            published_at: data["published_at"].as_str().unwrap_or("").to_string(),
            html_url: data["html_url"].as_str().unwrap_or("").to_string(),
            assets,
        }))
    }

    pub fn spawn_periodic(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(CHECK_INTERVAL_SECS));
            loop {
                ticker.tick().await;
                match self.check().await {
                    Ok(Some(release)) => {
                        info!(
                            version = %release.version,
                            current = %self.current_version,
                            "update_available"
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(error = %e, "update_check_failed");
                    }
                }
            }
        })
    }
}

impl Default for UpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

fn is_newer(remote: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse().ok()).collect() };
    let r = parse(remote);
    let c = parse(current);
    for i in 0..r.len().max(c.len()) {
        let rv = r.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if rv > cv {
            return true;
        }
        if rv < cv {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn version_comparison_different_lengths() {
        assert!(is_newer("1.0.0.1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0.1"));
    }

    #[test]
    fn asset_platform_detection() {
        let release = ReleaseInfo {
            tag_name: "v1.0.0".into(),
            version: "1.0.0".into(),
            name: "".into(),
            body: "".into(),
            published_at: "".into(),
            html_url: "".into(),
            assets: vec![
                ReleaseAsset {
                    name: "tune-server-linux-x86_64.tar.gz".into(),
                    browser_download_url: "".into(),
                    size: 0,
                    content_type: "".into(),
                },
                ReleaseAsset {
                    name: "tune-server-macos-aarch64.dmg".into(),
                    browser_download_url: "".into(),
                    size: 0,
                    content_type: "".into(),
                },
            ],
        };
        let asset = release.asset_for_platform();
        assert!(asset.is_some());
    }
}
