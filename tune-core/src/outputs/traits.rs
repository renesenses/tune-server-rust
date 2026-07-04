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
    /// The local audio thread has finished draining all audio data naturally
    /// (not via stop/skip). When true + state==Stopped, this is a definitive
    /// end-of-track that should trigger auto_next regardless of played_enough.
    pub ended_naturally: bool,
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
            ended_naturally: false,
        }
    }
}

pub struct PlayMedia<'a> {
    pub url: &'a str,
    pub mime_type: &'a str,
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub cover_url: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub file_size: Option<u64>,
    /// Local file path for outputs that can read directly (OAAT).
    pub file_path: Option<&'a str>,
    /// Audio sample rate in Hz (e.g. 176400 for DSD64->PCM).
    /// Used by DLNA renderers that require sampleFrequency in DIDL-Lite.
    pub sample_rate: Option<u32>,
    /// Audio bit depth (e.g. 24 for DSD->PCM transcoding).
    pub bit_depth: Option<u32>,
    /// Number of audio channels (e.g. 2 for stereo).
    pub channels: Option<u32>,
    /// True for infinite live streams (internet radio): the DIDL-Lite `<res>`
    /// must advertise a live/streaming source (DLNA.ORG_OP=00, senderPaced
    /// flags, no size/duration) rather than a seekable file, otherwise some
    /// renderers (Yamaha R-N2000A) accept SetAVTransportURI + Play but never
    /// produce sound.
    pub live_stream: bool,
}

impl Default for PlayMedia<'_> {
    fn default() -> Self {
        Self {
            url: "",
            mime_type: "",
            title: None,
            artist: None,
            album: None,
            cover_url: None,
            duration_ms: None,
            file_size: None,
            file_path: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            live_stream: false,
        }
    }
}

#[async_trait::async_trait]
pub trait OutputTarget: Send + Sync {
    fn name(&self) -> &str;
    fn device_id(&self) -> &str;
    fn output_type(&self) -> &str;

    fn as_any(&self) -> &dyn std::any::Any {
        // Default: not dowcastable. Implementations that need downcast override this.
        &()
    }

    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.play_media(&PlayMedia {
            url,
            mime_type,
            title,
            artist,
            ..Default::default()
        })
        .await
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

    fn host(&self) -> Option<&str> {
        None
    }

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
        self.set_next_url(media.url, media.mime_type, media.title, media.artist)
            .await
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        None
    }
}
