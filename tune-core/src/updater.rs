use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// List endpoints (NOT `/releases/latest`): `/latest` excludes prereleases, so a
// beta-channel build (e.g. "0.9.0-rc2") would never be offered newer RCs — and,
// because every stable release is a lower 0.8.x, it saw no update at all. Fetching
// the list lets us pick the newest release matching the client's channel.
const PROXY_RELEASES_URL: &str = "https://mozaiklabs.fr/api/tune/releases?per_page=20";
const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/renesenses/tune-server-rust/releases?per_page=20";
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
            client: crate::http::client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("tune-server")
                .build()
                .unwrap(),
            current_version: crate::version().to_string(),
        }
    }

    pub async fn check(&self) -> Result<Option<ReleaseInfo>, String> {
        // Try mozaiklabs.fr proxy first (no rate limit), fallback to GitHub
        let releases = match self.fetch_releases_json(PROXY_RELEASES_URL).await {
            Ok(d) => d,
            Err(proxy_err) => {
                warn!(error = %proxy_err, "proxy_check_failed, falling back to github");
                self.fetch_releases_json(GITHUB_RELEASES_URL).await?
            }
        };

        // Channel: a build whose own version carries a prerelease suffix
        // (e.g. "0.9.0-rc2") is on the beta channel and may install prereleases;
        // a stable build only ever sees stable (non-prerelease) releases.
        let on_beta = self.current_version.contains('-');

        // Pick the newest release the client is allowed to install.
        let mut best: Option<serde_json::Value> = None;
        let mut best_version = self.current_version.clone();
        for rel in releases {
            let is_pre = rel["prerelease"].as_bool().unwrap_or(false);
            if is_pre && !on_beta {
                continue; // stable clients never see prereleases
            }
            let version = rel["tag_name"].as_str().unwrap_or("").trim_start_matches('v');
            if version.is_empty() || !is_newer(version, &best_version) {
                continue;
            }
            best_version = version.to_string();
            best = Some(rel);
        }

        let Some(data) = best else {
            return Ok(None);
        };

        let tag = data["tag_name"].as_str().unwrap_or("").to_string();
        let version = tag.trim_start_matches('v').to_string();

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

    async fn fetch_releases_json(&self, url: &str) -> Result<Vec<serde_json::Value>, String> {
        let mut req = self.client.get(url);
        if url.contains("github.com") {
            if let Ok(token) = std::env::var("GITHUB_TOKEN") {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
        }
        let resp = req.send().await.map_err(|e| format!("request: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("api: {} {}", url, resp.status()));
        }
        resp.json().await.map_err(|e| format!("parse: {e}"))
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

/// Split a version into its release numbers and an optional prerelease rank.
/// "0.9.0-rc2" -> ([0,9,0], Some([2])); "0.9.0" -> ([0,9,0], None).
/// A `None` prerelease outranks any `Some` (semver: a final release is newer
/// than its own prereleases), so a "0.9.0-rc2" client is offered "0.9.0".
fn parse_version(s: &str) -> (Vec<u64>, Option<Vec<u64>>) {
    let (rel, pre) = match s.split_once('-') {
        Some((r, p)) => (r, Some(p)),
        None => (s, None),
    };
    let nums = |x: &str| -> Vec<u64> { x.split('.').filter_map(|p| p.parse().ok()).collect() };
    let pre_nums = pre.map(|p| {
        // "rc2" -> [2], "beta.1" -> [1]; digits only, non-digits are separators.
        p.split(|c: char| !c.is_ascii_digit())
            .filter_map(|t| t.parse::<u64>().ok())
            .collect::<Vec<u64>>()
    });
    (nums(rel), pre_nums)
}

fn cmp_nums(r: &[u64], c: &[u64]) -> std::cmp::Ordering {
    for i in 0..r.len().max(c.len()) {
        let rv = r.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        match rv.cmp(&cv) {
            std::cmp::Ordering::Equal => continue,
            ord => return ord,
        }
    }
    std::cmp::Ordering::Equal
}

fn is_newer(remote: &str, current: &str) -> bool {
    use std::cmp::Ordering;
    let (r_rel, r_pre) = parse_version(remote);
    let (c_rel, c_pre) = parse_version(current);
    match cmp_nums(&r_rel, &c_rel) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (r_pre, c_pre) {
            (None, None) => false,       // identical
            (None, Some(_)) => true,     // final > prerelease of same version
            (Some(_), None) => false,    // prerelease < its own final
            (Some(rp), Some(cp)) => cmp_nums(&rp, &cp) == Ordering::Greater,
        },
    }
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
    fn prerelease_precedence() {
        // A prerelease client is offered the final release of the same version.
        assert!(is_newer("0.9.0", "0.9.0-rc2"));
        // ...and newer prereleases of the same version.
        assert!(is_newer("0.9.0-rc3", "0.9.0-rc2"));
        // ...but not older/equal prereleases.
        assert!(!is_newer("0.9.0-rc1", "0.9.0-rc2"));
        assert!(!is_newer("0.9.0-rc2", "0.9.0-rc2"));
        // A final release is never "older" than its own prerelease.
        assert!(!is_newer("0.9.0-rc2", "0.9.0"));
        // The core bug: a lower stable must NOT look newer than a higher-versioned
        // prerelease (0.8.311 stable vs 0.9.0-rc2 beta).
        assert!(!is_newer("0.8.311", "0.9.0-rc2"));
        // A genuinely higher version wins regardless of prerelease state.
        assert!(is_newer("0.9.1", "0.9.0-rc2"));
        assert!(is_newer("0.10.0-rc1", "0.9.0"));
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
