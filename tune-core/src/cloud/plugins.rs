use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// A catalog row as served by `GET /api/v1/plugins` on mozaiklabs.fr (the
/// Laravel `Plugin::toApiArray()`). Every non-key field is defaulted: the
/// catalog schema has grown across eras (python-era rows lack `price`,
/// `downloads`, `rating`) and a single unknown/missing field must not turn
/// the whole catalog into an empty list (serde fails the entire Vec).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub slug: String,
    pub name: String,
    /// Human-facing name (`display_name` in the Laravel model).
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    /// `None` means free plugin.
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub category: String,
    #[serde(default, alias = "install_count")]
    pub downloads: u64,
    #[serde(default, alias = "vote_score")]
    pub rating: f64,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub installed_version: Option<String>,
    /// Legacy field kept for backward compat.
    #[serde(default, alias = "vote_count")]
    pub votes: i64,
    #[serde(default)]
    pub download_url: Option<String>,
    /// `wasm` rows are installable by the Rust server; python-era rows are not.
    #[serde(default)]
    pub platforms: Option<String>,
    #[serde(default)]
    pub install_type: Option<String>,
}

pub struct PluginMarketplace {
    base_url: String,
}

impl PluginMarketplace {
    pub fn new(base_url: Option<&str>) -> Self {
        Self {
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// List available plugins from the marketplace catalog.
    ///
    /// The Laravel store serves the catalog at `/api/v1/plugins` — the older
    /// `/api/v1/plugins/catalog` path never existed server-side, so this
    /// client silently returned an empty catalog (404 → `vec![]`).
    pub async fn list(&self) -> Vec<MarketplacePlugin> {
        let url = format!("{}/api/v1/plugins", self.base_url);
        let client = crate::http::client::shared();

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp.json().await.unwrap_or_default(),
            Ok(resp) => {
                debug!(status = %resp.status(), "marketplace_list_failed");
                vec![]
            }
            Err(e) => {
                debug!(error = %e, "marketplace_list_request_failed");
                vec![]
            }
        }
    }

    /// Fetch detail for a single marketplace plugin by slug (or package name).
    ///
    /// The Laravel store has no per-plugin detail endpoint, so this filters
    /// the catalog list client-side.
    pub async fn detail(&self, slug: &str) -> Option<MarketplacePlugin> {
        self.list()
            .await
            .into_iter()
            .find(|p| p.slug == slug || p.name == slug)
    }

    /// Download a plugin binary/archive by name.
    pub async fn download(&self, name: &str) -> Result<Vec<u8>, String> {
        let url = format!(
            "{}/api/v1/plugins/{}/download",
            self.base_url,
            urlencoding::encode(name)
        );
        let client = crate::http::client::long_timeout();

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("plugin download request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("plugin download failed: {}", resp.status()));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read plugin bytes: {e}"))?;

        info!(plugin = %name, size = bytes.len(), "marketplace_plugin_downloaded");
        Ok(bytes.to_vec())
    }

    /// Vote for a plugin (up or down).
    pub async fn vote(&self, name: &str, up: bool) -> Result<(), String> {
        let url = format!(
            "{}/api/v1/plugins/{}/vote",
            self.base_url,
            urlencoding::encode(name)
        );
        let client = crate::http::client::shared();

        let resp = client
            .post(&url)
            .json(&serde_json::json!({ "up": up }))
            .send()
            .await
            .map_err(|e| format!("plugin vote request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("plugin vote failed: {}", resp.status()));
        }

        info!(plugin = %name, up, "marketplace_plugin_voted");
        Ok(())
    }
}

impl Default for PluginMarketplace {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url() {
        let mp = PluginMarketplace::default();
        assert!(mp.base_url.contains("mozaiklabs.fr"));
    }

    #[test]
    fn custom_base_url() {
        let mp = PluginMarketplace::new(Some("http://localhost:3000/"));
        assert_eq!(mp.base_url, "http://localhost:3000");
    }
}
