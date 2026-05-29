use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
    Transitioning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputStatus {
    pub state: TransportState,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: f64,
    pub muted: bool,
    pub current_uri: Option<String>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
}

impl Default for OutputStatus {
    fn default() -> Self {
        Self {
            state: TransportState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            volume: 0.5,
            muted: false,
            current_uri: None,
            track_title: None,
            track_artist: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlayMedia<'a> {
    pub url: &'a str,
    pub mime_type: &'a str,
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub cover_url: Option<&'a str>,
}

#[async_trait::async_trait]
pub trait OutputTarget: Send + Sync {
    fn name(&self) -> &str;
    fn device_id(&self) -> &str;
    fn output_type(&self) -> &str;

    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.play_media(&PlayMedia {
            url, mime_type, title, artist, ..Default::default()
        }).await
    }

    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("not implemented".into())
    }

    async fn pause(&self) -> Result<(), String>;
    async fn resume(&self) -> Result<(), String>;
    async fn stop(&self) -> Result<(), String>;
    async fn seek(&self, position_ms: u64) -> Result<(), String>;
    async fn set_volume(&self, volume: f64) -> Result<(), String>;
    async fn set_mute(&self, muted: bool) -> Result<(), String>;
    async fn get_status(&self) -> Result<OutputStatus, String>;
    async fn is_available(&self) -> bool;

    async fn set_next_url(
        &self,
        _url: &str,
        _mime_type: &str,
        _title: Option<&str>,
        _artist: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.set_next_url(media.url, media.mime_type, media.title, media.artist).await
    }
}
