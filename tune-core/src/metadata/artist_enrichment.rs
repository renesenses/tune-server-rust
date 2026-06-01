use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

const CACHE_TTL_SECS: u64 = 3600;
const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr/api/v1/artists";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistData {
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

struct CacheEntry {
    data: ArtistData,
    fetched_at: Instant,
}

pub struct ArtistEnrichmentClient {
    base_url: String,
    cache: HashMap<String, CacheEntry>,
    timeout_secs: u64,
}

impl ArtistEnrichmentClient {
    pub fn new(base_url: Option<&str>, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            cache: HashMap::new(),
            timeout_secs,
        }
    }

    pub async fn get_artist(&mut self, mbid: &str) -> Option<ArtistData> {
        if let Some(cached) = self.cache_get(mbid) {
            return Some(cached);
        }
        let data = self.request(&format!("/{mbid}")).await?;
        let mut artist = ArtistData { fields: data };

        if let Some(inner) = artist.fields.get_mut("data")
            && let Some(img) = inner.get("image_url").and_then(|v| v.as_str())
                && img.starts_with("/storage/") {
                    let base = self
                        .base_url
                        .split("/api/")
                        .next()
                        .unwrap_or(&self.base_url);
                    let full = format!("{base}{img}");
                    inner["image_url"] = serde_json::json!(full);
                }

        self.cache_set(mbid, artist.clone());
        Some(artist)
    }

    pub async fn get_bio(&mut self, mbid: &str, lang: &str) -> Option<String> {
        let data = self
            .request_with_params(&format!("/{mbid}/bio"), &[("lang", lang)])
            .await?;
        data.get("bio")
            .or_else(|| data.get("text"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub async fn get_similar(&mut self, mbid: &str) -> Vec<serde_json::Value> {
        match self.request(&format!("/{mbid}/similar")).await {
            Some(serde_json::Value::Array(arr)) => arr,
            Some(obj) => obj
                .get("artists")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            None => vec![],
        }
    }

    pub async fn search(&mut self, query: &str) -> Vec<serde_json::Value> {
        match self.request_with_params("/search", &[("q", query)]).await {
            Some(serde_json::Value::Array(arr)) => arr,
            Some(obj) => obj
                .get("artists")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
            None => vec![],
        }
    }

    pub async fn refresh(&mut self, mbid: &str) -> Option<ArtistData> {
        self.cache.remove(mbid);
        let url = format!("{}/{mbid}/refresh", self.base_url);
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        let artist = ArtistData { fields: data };
        self.cache_set(mbid, artist.clone());
        Some(artist)
    }

    fn cache_get(&self, mbid: &str) -> Option<ArtistData> {
        let entry = self.cache.get(mbid)?;
        if entry.fetched_at.elapsed().as_secs() > CACHE_TTL_SECS {
            return None;
        }
        Some(entry.data.clone())
    }

    fn cache_set(&mut self, mbid: &str, data: ArtistData) {
        self.cache.insert(
            mbid.to_string(),
            CacheEntry {
                data,
                fetched_at: Instant::now(),
            },
        );
    }

    async fn request(&self, path: &str) -> Option<serde_json::Value> {
        let url = format!("{}{path}", self.base_url);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        resp.json().await.ok()
    }

    async fn request_with_params(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Option<serde_json::Value> {
        let url = format!("{}{path}", self.base_url);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .query(params)
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .ok()?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return None;
        }
        resp.json().await.ok()
    }
}

impl Default for ArtistEnrichmentClient {
    fn default() -> Self {
        Self::new(None, 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_base_url() {
        let client = ArtistEnrichmentClient::default();
        assert!(client.base_url.contains("mozaiklabs.fr"));
    }

    #[test]
    fn cache_miss() {
        let client = ArtistEnrichmentClient::default();
        assert!(client.cache_get("nonexistent-mbid").is_none());
    }

    #[test]
    fn cache_set_and_get() {
        let mut client = ArtistEnrichmentClient::default();
        let data = ArtistData {
            fields: serde_json::json!({"name": "Test"}),
        };
        client.cache_set("abc-123", data.clone());
        let cached = client.cache_get("abc-123");
        assert!(cached.is_some());
    }

    #[test]
    fn custom_url() {
        let client = ArtistEnrichmentClient::new(Some("http://localhost:3000/api"), 10);
        assert_eq!(client.base_url, "http://localhost:3000/api");
    }
}
