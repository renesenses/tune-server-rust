use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlaying {
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_path: Option<String>,
    pub duration_ms: i64,
    pub source: String,
    pub source_id: Option<String>,
    pub stream_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneState {
    pub zone_id: i64,
    pub state: PlayState,
    pub now_playing: Option<NowPlaying>,
    pub position_ms: i64,
    pub volume: f64,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub queue_position: i64,
    pub queue_length: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

impl Default for ZoneState {
    fn default() -> Self {
        Self {
            zone_id: 0,
            state: PlayState::Stopped,
            now_playing: None,
            position_ms: 0,
            volume: 0.5,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            queue_position: 0,
            queue_length: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaybackEvent {
    pub event: String,
    pub zone_id: i64,
    pub data: serde_json::Value,
}

pub struct PlaybackManager {
    zones: Arc<Mutex<HashMap<i64, ZoneState>>>,
    event_tx: broadcast::Sender<PlaybackEvent>,
}

impl Default for PlaybackManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackManager {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            zones: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackEvent> {
        self.event_tx.subscribe()
    }

    pub async fn get_state(&self, zone_id: i64) -> ZoneState {
        let zones = self.zones.lock().await;
        zones.get(&zone_id).cloned().unwrap_or(ZoneState {
            zone_id,
            ..Default::default()
        })
    }

    pub async fn all_states(&self) -> Vec<ZoneState> {
        let zones = self.zones.lock().await;
        zones.values().cloned().collect()
    }

    pub async fn play(&self, zone_id: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.state = PlayState::Playing;
        state.position_ms = 0;
        state.now_playing = Some(np.clone());

        self.emit(PlaybackEvent {
            event: "play".into(),
            zone_id,
            data: serde_json::json!({
                "title": np.title,
                "artist_name": np.artist_name,
                "album_title": np.album_title,
                "cover_path": np.cover_path,
                "duration_ms": np.duration_ms,
                "source": np.source,
                "source_id": np.source_id,
            }),
        });
    }

    pub async fn pause(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Paused;
        }
        self.emit(PlaybackEvent {
            event: "pause".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn resume(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Playing;
        }
        self.emit(PlaybackEvent {
            event: "resume".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn stop(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Stopped;
            state.now_playing = None;
            state.position_ms = 0;
        }
        self.emit(PlaybackEvent {
            event: "stop".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn seek(&self, zone_id: i64, position_ms: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.position_ms = position_ms;
        }
        self.emit(PlaybackEvent {
            event: "seek".into(),
            zone_id,
            data: serde_json::json!({ "position_ms": position_ms }),
        });
    }

    pub async fn set_volume(&self, zone_id: i64, volume: f64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.volume = volume.clamp(0.0, 1.0);
        }
        self.emit(PlaybackEvent {
            event: "volume".into(),
            zone_id,
            data: serde_json::json!({ "volume": volume }),
        });
    }

    pub async fn set_shuffle(&self, zone_id: i64, enabled: bool) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.shuffle = enabled;
        }
    }

    pub async fn set_repeat(&self, zone_id: i64, mode: RepeatMode) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.repeat = mode;
        }
    }

    pub async fn update_position(&self, zone_id: i64, position_ms: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.position_ms = position_ms;
        }
    }

    pub async fn update_queue_info(&self, zone_id: i64, position: i64, length: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.queue_position = position;
            state.queue_length = length;
        }
    }

    pub fn emit_position(&self, zone_id: i64, position_ms: i64) {
        self.emit(PlaybackEvent {
            event: "position".into(),
            zone_id,
            data: serde_json::json!({ "position_ms": position_ms }),
        });
    }

    /// Update the NowPlaying metadata for a zone without resetting position.
    /// Used for radio streams where the track info changes while playing.
    pub async fn update_now_playing(&self, zone_id: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.now_playing = Some(np.clone());
        }
        self.emit(PlaybackEvent {
            event: "now_playing".into(),
            zone_id,
            data: serde_json::json!({
                "title": np.title,
                "artist_name": np.artist_name,
                "album_title": np.album_title,
                "cover_path": np.cover_path,
                "duration_ms": np.duration_ms,
                "source": np.source,
                "source_id": np.source_id,
            }),
        });
    }

    fn emit(&self, event: PlaybackEvent) {
        let _ = self.event_tx.send(event);
    }
}
