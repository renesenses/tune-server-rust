use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tracing::{debug, warn};

use crate::remote_discovery::PeerServer;

pub struct RemoteProxy {
    client: Client,
}

impl Default for RemoteProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteProxy {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    fn base_url(peer: &PeerServer) -> String {
        format!("http://{}:{}", peer.address, peer.port)
    }

    pub async fn get_info(&self, peer: &PeerServer) -> Result<Value, String> {
        let url = format!("{}/api/system/info", Self::base_url(peer));
        self.get_json(&url).await
    }

    pub async fn get_zones(&self, peer: &PeerServer) -> Result<Value, String> {
        let url = format!("{}/api/zones", Self::base_url(peer));
        self.get_json(&url).await
    }

    pub async fn get_library(
        &self,
        peer: &PeerServer,
        limit: usize,
        offset: usize,
    ) -> Result<Value, String> {
        let url = format!(
            "{}/api/library/tracks?limit={}&offset={}",
            Self::base_url(peer),
            limit,
            offset
        );
        self.get_json(&url).await
    }

    pub async fn search(
        &self,
        peer: &PeerServer,
        query: &str,
        limit: usize,
    ) -> Result<Value, String> {
        let url = format!(
            "{}/api/search?q={}&limit={}",
            Self::base_url(peer),
            urlencoding::encode(query),
            limit
        );
        self.get_json(&url).await
    }

    pub async fn play_track(
        &self,
        peer: &PeerServer,
        zone_id: i64,
        track_id: i64,
    ) -> Result<Value, String> {
        let url = format!("{}/api/playback/play", Self::base_url(peer));
        let body = serde_json::json!({
            "zone_id": zone_id,
            "track_id": track_id,
        });
        self.post_json(&url, &body).await
    }

    pub async fn playback_command(
        &self,
        peer: &PeerServer,
        zone_id: i64,
        command: &str,
    ) -> Result<Value, String> {
        let url = format!("{}/api/playback/{}", Self::base_url(peer), command);
        let body = serde_json::json!({ "zone_id": zone_id });
        self.post_json(&url, &body).await
    }

    pub async fn get_stream_url(
        &self,
        peer: &PeerServer,
        track_id: i64,
    ) -> Result<String, String> {
        let base = Self::base_url(peer);
        Ok(format!("{base}/api/stream/{track_id}"))
    }

    pub async fn get_artwork_url(
        &self,
        peer: &PeerServer,
        track_id: i64,
    ) -> String {
        let base = Self::base_url(peer);
        format!("{base}/api/artwork/track/{track_id}")
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("proxy get: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("remote returned {}", resp.status()));
        }

        resp.json().await.map_err(|e| format!("proxy parse: {e}"))
    }

    async fn post_json(&self, url: &str, body: &Value) -> Result<Value, String> {
        let resp = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("proxy post: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("remote returned {}", resp.status()));
        }

        resp.json().await.map_err(|e| format!("proxy parse: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer() -> PeerServer {
        PeerServer {
            server_id: "test-id".into(),
            name: "Test".into(),
            address: "192.168.1.10".into(),
            port: 3000,
            version: "1.0.0".into(),
            last_seen: 0,
        }
    }

    #[test]
    fn base_url() {
        let peer = test_peer();
        assert_eq!(RemoteProxy::base_url(&peer), "http://192.168.1.10:3000");
    }

    #[tokio::test]
    async fn stream_url() {
        let proxy = RemoteProxy::new();
        let peer = test_peer();
        let url = proxy.get_stream_url(&peer, 42).await.unwrap();
        assert_eq!(url, "http://192.168.1.10:3000/api/stream/42");
    }

    #[tokio::test]
    async fn artwork_url() {
        let proxy = RemoteProxy::new();
        let peer = test_peer();
        let url = proxy.get_artwork_url(&peer, 42).await;
        assert_eq!(url, "http://192.168.1.10:3000/api/artwork/track/42");
    }
}
