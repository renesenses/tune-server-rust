use serde::{Deserialize, Serialize};
use tracing::{debug, info};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    /// `None` means free plugin.
    pub price: Option<f64>,
    pub category: String,
    pub downloads: u64,
    pub rating: f64,
    #[serde(default)]
    pub installed: bool,
    pub installed_version: Option<String>,
    /// Legacy field kept for backward compat.
    #[serde(default)]
    pub votes: i64,
    pub download_url: Option<String>,
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
    pub async fn list(&self) -> Vec<MarketplacePlugin> {
        let url = format!("{}/api/v1/plugins/catalog", self.base_url);
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

    /// Fetch detail for a single marketplace plugin by slug.
    pub async fn detail(&self, slug: &str) -> Option<MarketplacePlugin> {
        let url = format!(
            "{}/api/v1/plugins/catalog/{}",
            self.base_url,
            urlencoding::encode(slug)
        );
        let client = crate::http::client::shared();

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp.json().await.ok(),
            Ok(resp) => {
                debug!(slug, status = %resp.status(), "marketplace_detail_failed");
                None
            }
            Err(e) => {
                debug!(slug, error = %e, "marketplace_detail_request_failed");
                None
            }
        }
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
