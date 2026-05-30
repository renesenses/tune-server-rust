use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
use tracing::info;

use super::traits::*;

pub struct SqueezeboxOutput {
    name: String,
    device_id: String,
    lms_host: String,
    lms_port: u16,
    client: Client,
}

impl SqueezeboxOutput {
    pub fn new(name: String, device_id: String, lms_host: String, lms_port: u16) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            name,
            device_id,
            lms_host,
            lms_port,
            client,
        }
    }

    fn jsonrpc_url(&self) -> String {
        format!("http://{}:{}/jsonrpc.js", self.lms_host, self.lms_port)
    }

    async fn lms_request(&self, cmd: Vec<Value>) -> Result<Value, String> {
        let body = json!({
            "id": 1,
            "method": "slim.request",
            "params": [&self.device_id, cmd],
        });
        let resp = self
            .client
            .post(self.jsonrpc_url())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    format!(
                        "LMS connection failed ({}:{}): {e}",
                        self.lms_host, self.lms_port
                    )
                } else if e.is_timeout() {
                    format!("LMS timeout ({}:{})", self.lms_host, self.lms_port)
                } else {
                    format!("lms request: {e}")
                }
            })?;
        let text = resp.text().await.map_err(|e| format!("lms read: {e}"))?;
        if text.is_empty() {
            return Err(format!(
                "LMS returned empty response ({}:{}). Check that the server is a Squeezebox/LMS instance.",
                self.lms_host, self.lms_port
            ));
        }
        let json: Value = serde_json::from_str(&text)
            .map_err(|e| format!("JSON-parse: {e} (body: {})", &text[..text.len().min(200)]))?;
        Ok(json.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn player_status(&self) -> Result<Value, String> {
        self.lms_request(vec![
            json!("status"),
            json!(0),
            json!(100),
            json!("tags:adlNJ"),
        ])
        .await
    }
}

#[async_trait::async_trait]
impl OutputTarget for SqueezeboxOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "squeezebox"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.lms_host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        info!(player = %self.device_id, url = media.url, "squeezebox_play");
        self.lms_request(vec![json!("playlist"), json!("play"), json!(media.url)])
            .await?;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.lms_request(vec![json!("pause"), json!(1)]).await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.lms_request(vec![json!("pause"), json!(0)]).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.lms_request(vec![json!("stop")]).await?;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let secs = position_ms / 1000;
        self.lms_request(vec![json!("time"), json!(secs)]).await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let vol = (volume * 100.0).round().clamp(0.0, 100.0) as u8;
        self.lms_request(vec![json!("mixer"), json!("volume"), json!(vol)])
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { 1 } else { 0 };
        self.lms_request(vec![json!("mixer"), json!("muting"), json!(val)])
            .await?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let result = self.player_status().await?;
        let mode = result["mode"].as_str().unwrap_or("stop");
        let state = match mode {
            "play" => TransportState::Playing,
            "pause" => TransportState::Paused,
            _ => TransportState::Stopped,
        };

        let position_ms = result["time"]
            .as_f64()
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);
        let duration_ms = result["duration"]
            .as_f64()
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);
        let volume = result["mixer volume"]
            .as_f64()
            .or_else(|| result["mixer_volume"].as_f64())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);

        let current_track = result
            .get("playlist_loop")
            .and_then(|pl| pl.as_array())
            .and_then(|arr| arr.first());

        let current_uri = result["current_title"].as_str().map(|s| s.to_string());
        let track_title = current_track
            .and_then(|t| t["title"].as_str())
            .map(|s| s.to_string());
        let track_artist = current_track
            .and_then(|t| t["artist"].as_str())
            .map(|s| s.to_string());

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
        self.player_status().await.is_ok()
    }

    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        _title: Option<&str>,
        _artist: Option<&str>,
    ) -> Result<(), String> {
        self.lms_request(vec![json!("playlist"), json!("add"), json!(url)])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_url() {
        let sb = SqueezeboxOutput::new(
            "Kitchen".into(),
            "aa:bb:cc:dd:ee:ff".into(),
            "192.168.1.100".into(),
            9000,
        );
        assert_eq!(sb.jsonrpc_url(), "http://192.168.1.100:9000/jsonrpc.js");
    }

    #[test]
    fn output_type() {
        let sb = SqueezeboxOutput::new("Test".into(), "id".into(), "localhost".into(), 9000);
        assert_eq!(sb.output_type(), "squeezebox");
    }
}
