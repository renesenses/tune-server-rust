pub mod auto_dj;
pub mod crossfade;
pub mod dj_player;
pub mod gapless;
pub mod queue;
pub mod radio_handler;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub genre: Option<String>,
    pub year: Option<i32>,
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
    /// Monotonically increasing counter bumped on each `play()` call.
    /// The poller uses this to detect track changes and reset its state
    /// (peak_position, gapless flags, etc.) so stale data from the
    /// previous track cannot trigger false advances.
    #[serde(default)]
    pub track_generation: u64,
    /// Timestamp of the last seek operation.  The poller checks this and
    /// suppresses stale position updates from the output for a brief grace
    /// period so the UI doesn't snap back to the pre-seek position.
    #[serde(skip)]
    pub last_seek_at: Option<Instant>,
    /// Timestamp of the last user-initiated volume change.  The poller
    /// ignores renderer-reported volume for a grace period to prevent
    /// the slider from bouncing back on slow DLNA renderers.
    #[serde(skip)]
    pub last_volume_set_at: Option<Instant>,
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
            track_generation: 0,
            last_seek_at: None,
            last_volume_set_at: None,
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

    /// Restore a saved playback position into the zone state.
    /// Called on startup to remember where playback left off.
    pub async fn restore_position(&self, zone_id: i64, position_ms: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.position_ms = position_ms;
        state.now_playing = Some(np);
        state.state = PlayState::Stopped;
    }

    pub async fn all_states(&self) -> Vec<ZoneState> {
        let zones = self.zones.lock().await;
        zones.values().cloned().collect()
    }

    pub async fn bump_generation(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.track_generation = state.track_generation.wrapping_add(1);
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
        state.track_generation = state.track_generation.wrapping_add(1);
        // Preserve last_seek_at if a seek just happened (< 5s ago) — the
        // orchestrator recreates the stream during seek, which calls play().
        // Clearing it here would remove the seek grace period from the poller.
        let is_recent_seek = state
            .last_seek_at
            .map(|t| t.elapsed().as_secs() < 5)
            .unwrap_or(false);
        if !is_recent_seek {
            state.last_seek_at = None;
        }

        self.emit(PlaybackEvent {
            event: "started".into(),
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
            event: "paused".into(),
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
            event: "resumed".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn stop(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        let np_data = if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Stopped;
            state.last_seek_at = None;
            // Keep position_ms and now_playing so the UI shows where
            // playback left off and can resume from the same position.
            state.now_playing.as_ref().map(|np| {
                serde_json::json!({
                    "track_id": np.track_id,
                    "title": np.title,
                    "artist_name": np.artist_name,
                    "album_title": np.album_title,
                    "cover_path": np.cover_path,
                    "duration_ms": np.duration_ms,
                    "source": np.source,
                    "source_id": np.source_id,
                })
            })
        } else {
            None
        };
        self.emit(PlaybackEvent {
            event: "stopped".into(),
            zone_id,
            data: np_data.unwrap_or(serde_json::json!({})),
        });
    }

    /// Stop playback and clear the now_playing metadata entirely.
    /// Used when the queue is cleared — there is nothing to resume.
    pub async fn stop_and_clear(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Stopped;
            state.now_playing = None;
            state.position_ms = 0;
        }
        self.emit(PlaybackEvent {
            event: "stopped".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn seek(&self, zone_id: i64, position_ms: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.position_ms = position_ms;
            state.last_seek_at = Some(Instant::now());
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

    pub async fn mark_volume_changed(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.last_volume_set_at = Some(Instant::now());
        }
    }

    pub async fn set_mute(&self, zone_id: i64, muted: bool) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.muted = muted;
        }
        self.emit(PlaybackEvent {
            event: "muted".into(),
            zone_id,
            data: serde_json::json!({ "muted": muted }),
        });
    }

    pub async fn set_shuffle(&self, zone_id: i64, enabled: bool) {
        {
            let mut zones = self.zones.lock().await;
            if let Some(state) = zones.get_mut(&zone_id) {
                state.shuffle = enabled;
            }
        }
        self.emit(PlaybackEvent {
            event: "shuffle".into(),
            zone_id,
            data: serde_json::json!({ "enabled": enabled }),
        });
    }

    pub async fn set_repeat(&self, zone_id: i64, mode: RepeatMode) {
        {
            let mut zones = self.zones.lock().await;
            if let Some(state) = zones.get_mut(&zone_id) {
                state.repeat = mode;
            }
        }
        self.emit(PlaybackEvent {
            event: "repeat".into(),
            zone_id,
            data: serde_json::json!({ "mode": mode }),
        });
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
            event: "track_changed".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_shuffle_emits_event() {
        let pm = PlaybackManager::new();
        let mut rx = pm.subscribe();
        pm.set_shuffle(7, true).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event, "shuffle");
        assert_eq!(ev.zone_id, 7);
        assert_eq!(ev.data["enabled"], true);
    }

    #[tokio::test]
    async fn set_repeat_emits_event() {
        let pm = PlaybackManager::new();
        let mut rx = pm.subscribe();
        pm.set_repeat(3, RepeatMode::All).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event, "repeat");
        assert_eq!(ev.zone_id, 3);
        assert_eq!(ev.data["mode"], "all");
    }
}
