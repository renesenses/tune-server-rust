use reqwest::Client;
use tracing::info;

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

pub struct BluosOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    client: Client,
}

impl BluosOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            client: crate::http::client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    async fn api_get(&self, path: &str, params: &[(&str, &str)]) -> Result<String, String> {
        let url = format!("{}/{}", self.base_url(), path);
        self.client
            .get(&url)
            .query(params)
            .send()
            .await
            .map_err(|e| format!("bluos {path}: {e}"))?
            .text()
            .await
            .map_err(|e| format!("bluos read {path}: {e}"))
    }
}

#[async_trait::async_trait]
impl OutputTarget for BluosOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "bluos"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Clear the Node's internal play queue before starting a new track.
        // BluOS keeps its own queue: set_next_media (/Add?prepend=1) stages the
        // gapless track into it, and /Play?url= does NOT purge leftover entries.
        // Without this, tracks from a previous album stayed queued and the Node
        // auto-advanced onto them at every track transition (Scordia: a new CD
        // plays track 1, then jumps to the previous CD's tracks — "in memory"
        // yet absent from Tune's own queue/history, because they live on the
        // Node). Fire-and-forget: a failed Clear must not block playback. Gapless
        // within the album still works — set_next_media rebuilds a fresh queue.
        let _ = self.api_get("Clear", &[]).await;

        // BluOS expects the url parameter without re-encoding — .query()
        // would double-encode http:// in the stream URL, causing silent failure.
        let mut play_url = format!("{}/Play?url={}", self.base_url(), media.url);
        if let Some(t) = media.title {
            play_url.push_str(&format!("&title={}", urlencoding::encode(t)));
        }
        if let Some(a) = media.artist {
            play_url.push_str(&format!("&artist={}", urlencoding::encode(a)));
        }
        if let Some(al) = media.album {
            play_url.push_str(&format!("&album={}", urlencoding::encode(al)));
        }
        if let Some(img) = media.cover_url {
            play_url.push_str(&format!("&image={}", urlencoding::encode(img)));
        }
        self.client
            .get(&play_url)
            .send()
            .await
            .map_err(|e| format!("bluos Play: {e}"))?
            .text()
            .await
            .map_err(|e| format!("bluos Play read: {e}"))?;
        info!(
            device = %self.name,
            url = media.url,
            title = media.title.unwrap_or(""),
            artist = media.artist.unwrap_or(""),
            album = media.album.unwrap_or(""),
            image = media.cover_url.unwrap_or(""),
            "bluos_play"
        );
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.api_get("Pause", &[]).await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.api_get("Play", &[]).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.api_get("Stop", &[]).await?;
        info!(device = %self.name, "bluos_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let seconds = (position_ms / 1000).to_string();
        self.api_get("Play", &[("seek", &seconds)]).await?;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let level = (volume * 100.0).round().clamp(0.0, 100.0) as u32;
        self.api_get("Volume", &[("level", &level.to_string())])
            .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "on" } else { "off" };
        self.api_get("Volume", &[("mute", val)]).await?;
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let xml = self.api_get("Status", &[]).await?;

        let state = match extract_tag(&xml, "state")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "play" | "stream" => TransportState::Playing,
            "pause" => TransportState::Paused,
            "connecting" | "buffering" => TransportState::Transitioning,
            _ => TransportState::Stopped,
        };

        let position_ms = extract_tag(&xml, "secs")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let duration_ms = extract_tag(&xml, "totlen")
            .and_then(|s| s.parse::<f64>().ok())
            .map(|s| (s * 1000.0) as u64)
            .unwrap_or(0);

        let volume = extract_tag(&xml, "volume")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5);

        let muted = extract_tag(&xml, "mute")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);

        let current_uri = extract_tag(&xml, "streamUrl").or_else(|| extract_tag(&xml, "song"));

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title: extract_tag(&xml, "title1"),
            track_artist: extract_tag(&xml, "artist"),
            ended_naturally: false,
        })
    }

    async fn is_available(&self) -> bool {
        self.client
            .get(format!("{}/Status", self.base_url()))
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // BluOS /Add?prepend=1 queues the next track for gapless playback.
        // Raw URL construction (no .query()) to avoid double-encoding, same as play_media.
        let mut add_url = format!("{}/Add?url={}&prepend=1", self.base_url(), media.url);
        if let Some(t) = media.title {
            add_url.push_str(&format!("&title={}", urlencoding::encode(t)));
        }
        if let Some(a) = media.artist {
            add_url.push_str(&format!("&artist={}", urlencoding::encode(a)));
        }
        if let Some(al) = media.album {
            add_url.push_str(&format!("&album={}", urlencoding::encode(al)));
        }
        if let Some(img) = media.cover_url {
            add_url.push_str(&format!("&image={}", urlencoding::encode(img)));
        }
        self.client
            .get(&add_url)
            .send()
            .await
            .map_err(|e| format!("bluos Add: {e}"))?;
        info!(
            device = %self.name,
            url = media.url,
            title = media.title.unwrap_or(""),
            artist = media.artist.unwrap_or(""),
            album = media.album.unwrap_or(""),
            image = media.cover_url.unwrap_or(""),
            "bluos_set_next"
        );
        Ok(())
    }
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let text = xml[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_xml() {
        let xml = r#"<status><state>play</state><secs>123.4</secs><totlen>300.0</totlen><volume>50</volume><title1>Test Song</title1><artist>Test Artist</artist></status>"#;
        assert_eq!(extract_tag(xml, "state"), Some("play".into()));
        assert_eq!(extract_tag(xml, "secs"), Some("123.4".into()));
        assert_eq!(extract_tag(xml, "volume"), Some("50".into()));
        assert_eq!(extract_tag(xml, "title1"), Some("Test Song".into()));
    }

    #[test]
    fn parse_empty_tags() {
        let xml = "<status><state>stop</state><secs></secs></status>";
        assert_eq!(extract_tag(xml, "state"), Some("stop".into()));
        assert_eq!(extract_tag(xml, "secs"), None);
    }

    #[test]
    fn parse_mute_status() {
        let xml = "<status><mute>on</mute><volume>42</volume></status>";
        let muted = extract_tag(xml, "mute")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        assert!(muted);
    }
}
