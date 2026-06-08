use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

/// Default HQPlayer HTTP Control API port.
pub const HQPLAYER_DEFAULT_PORT: u16 = 4321;

pub struct HqplayerOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    client: Client,
}

impl HqplayerOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    async fn api_get(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url(), path);
        debug!(url = %url, "hqplayer_api_get");
        self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("hqplayer GET {path}: {e}"))?
            .text()
            .await
            .map_err(|e| format!("hqplayer read {path}: {e}"))
    }

    async fn api_post(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url(), path);
        debug!(url = %url, "hqplayer_api_post");
        self.client
            .post(&url)
            .send()
            .await
            .map_err(|e| format!("hqplayer POST {path}: {e}"))?
            .text()
            .await
            .map_err(|e| format!("hqplayer read {path}: {e}"))
    }
}

/// HQPlayer status response (JSON).
#[derive(Debug, Deserialize, Default)]
struct HqpStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    position: f64,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    volume: Option<f64>,
    #[serde(default)]
    track: Option<HqpTrack>,
}

#[derive(Debug, Deserialize, Default)]
struct HqpTrack {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

fn parse_transport_state(state: &str) -> TransportState {
    match state.to_lowercase().as_str() {
        "playing" | "play" => TransportState::Playing,
        "paused" | "pause" => TransportState::Paused,
        "stopped" | "stop" => TransportState::Stopped,
        "transitioning" | "buffering" => TransportState::Transitioning,
        _ => TransportState::Stopped,
    }
}

#[async_trait::async_trait]
impl OutputTarget for HqplayerOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "hqplayer"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        info!(device = %self.name, url = media.url, "hqplayer_play");

        // Set source URI first
        let encoded = urlencoding::encode(media.url);
        self.api_post(&format!("/api/source/uri?uri={encoded}"))
            .await
            .map_err(|e| {
                warn!(error = %e, "hqplayer_set_source_failed");
                e
            })?;

        // Then issue play
        self.api_post("/api/transport/play").await.map_err(|e| {
            warn!(error = %e, "hqplayer_play_failed");
            e
        })?;

        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.api_post("/api/transport/pause").await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.api_post("/api/transport/play").await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.api_post("/api/transport/stop").await?;
        info!(device = %self.name, "hqplayer_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = position_ms as f64 / 1000.0;
        self.api_post(&format!("/api/transport/seek?position={seconds:.1}"))
            .await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        self.api_post(&format!("/api/transport/volume?level={level}"))
            .await?;
        Ok(())
    }

    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        // HQPlayer HTTP API does not support mute toggle
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let body = self.api_get("/api/status").await?;
        let hqp: HqpStatus = serde_json::from_str(&body).unwrap_or_default();

        let state = parse_transport_state(&hqp.state);
        let position_ms = (hqp.position * 1000.0) as u64;
        let duration_ms = (hqp.duration * 1000.0) as u64;
        let volume = hqp.volume.map(|v| v / 100.0).unwrap_or(1.0);

        let (track_title, track_artist, current_uri) = match hqp.track {
            Some(t) => (t.title, t.artist, t.uri),
            None => (None, None, None),
        };

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted: false,
            current_uri,
            track_title,
            track_artist,
        })
    }

    async fn is_available(&self) -> bool {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        let url = format!("{}/api/status", self.base_url());
        client
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port() {
        assert_eq!(HQPLAYER_DEFAULT_PORT, 4321);
    }

    #[test]
    fn output_type() {
        let hqp = HqplayerOutput::new("HQPlayer".into(), "hqp-1".into(), "localhost".into(), 4321);
        assert_eq!(hqp.output_type(), "hqplayer");
    }

    #[test]
    fn base_url() {
        let hqp = HqplayerOutput::new(
            "HQPlayer".into(),
            "hqp-1".into(),
            "192.168.1.100".into(),
            4321,
        );
        assert_eq!(hqp.base_url(), "http://192.168.1.100:4321");
    }

    #[test]
    fn host_returned() {
        let hqp = HqplayerOutput::new(
            "HQPlayer".into(),
            "hqp-1".into(),
            "192.168.1.100".into(),
            4321,
        );
        assert_eq!(hqp.host(), Some("192.168.1.100"));
    }

    #[test]
    fn parse_states() {
        assert_eq!(parse_transport_state("playing"), TransportState::Playing);
        assert_eq!(parse_transport_state("paused"), TransportState::Paused);
        assert_eq!(parse_transport_state("stopped"), TransportState::Stopped);
        assert_eq!(parse_transport_state("PLAYING"), TransportState::Playing);
        assert_eq!(parse_transport_state("play"), TransportState::Playing);
        assert_eq!(
            parse_transport_state("buffering"),
            TransportState::Transitioning
        );
        assert_eq!(parse_transport_state("unknown"), TransportState::Stopped);
    }

    #[test]
    fn parse_status_json() {
        let json = r#"{"state":"playing","position":42.5,"duration":180.0,"volume":75,"track":{"title":"Test Song","artist":"Test Artist","uri":"http://example.com/track.flac"}}"#;
        let hqp: HqpStatus = serde_json::from_str(json).unwrap();
        assert_eq!(hqp.state, "playing");
        assert_eq!(hqp.position, 42.5);
        assert_eq!(hqp.duration, 180.0);
        assert_eq!(hqp.volume, Some(75.0));
        let track = hqp.track.unwrap();
        assert_eq!(track.title.as_deref(), Some("Test Song"));
        assert_eq!(track.artist.as_deref(), Some("Test Artist"));
        assert_eq!(track.uri.as_deref(), Some("http://example.com/track.flac"));
    }

    #[test]
    fn parse_empty_status() {
        let json = r#"{}"#;
        let hqp: HqpStatus = serde_json::from_str(json).unwrap();
        assert_eq!(hqp.state, "");
        assert_eq!(hqp.position, 0.0);
        assert_eq!(hqp.volume, None);
        assert!(hqp.track.is_none());
    }
}
