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
    /// Materialised shuffle order: a permutation of the queue indices
    /// `[0, queue_length)`. Empty when shuffle is off. Regenerated when shuffle
    /// is enabled or the queue length changes, and re-synced on every position
    /// update. `next_position` follows this order so shuffle plays every track
    /// exactly once per cycle (repeat-off stops at the end; repeat-all loops).
    #[serde(skip)]
    pub shuffle_order: Vec<usize>,
    /// Current index into `shuffle_order` (-1 before the first track). The next
    /// shuffle track is `shuffle_order[shuffle_index + 1]`.
    #[serde(skip)]
    pub shuffle_index: i64,
    /// Monotonically increasing counter bumped on each `play()` call.
    /// The poller uses this to detect track changes and reset its state
    /// (peak_position, gapless flags, etc.) so stale data from the
    /// previous track cannot trigger false advances.
    #[serde(default)]
    pub track_generation: u64,
    /// Monotonic play-request counter, bumped only when a new play is issued
    /// for this zone (`bump_generation`). Unlike `track_generation` — which the
    /// poller also bumps on recovery — this changes ONLY on an actual new play,
    /// so the orchestrator can detect that a newer play superseded an in-flight
    /// one (slow resolve) and skip sending a second, overlapping stream.
    #[serde(default)]
    pub play_seq: u64,
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
            shuffle_order: Vec::new(),
            shuffle_index: -1,
            track_generation: 0,
            play_seq: 0,
            last_seek_at: None,
            last_volume_set_at: None,
        }
    }
}

/// Build a materialised shuffle order: a Fisher-Yates permutation of
/// `[0, length)` with `current` moved to index 0, so the first advance goes to
/// a different track than the one playing. Seeded from the wall clock via a
/// xorshift64 PRNG (no `rand` crate dependency).
pub(crate) fn generate_shuffle_order(length: usize, current: usize) -> Vec<usize> {
    if length == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..length).collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    // Fisher-Yates using a xorshift64 PRNG.
    for i in (1..length).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    if current < length {
        if let Some(pos) = order.iter().position(|&x| x == current) {
            order.swap(0, pos);
        }
    }
    order
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

    pub async fn bump_generation(&self, zone_id: i64) -> u64 {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.track_generation = state.track_generation.wrapping_add(1);
        state.play_seq = state.play_seq.wrapping_add(1);
        state.play_seq
    }

    /// Current play-request sequence for a zone (0 if never played). Compared
    /// against the value captured at play start to detect that a newer play
    /// superseded an in-flight one before it sends output.
    pub async fn current_play_seq(&self, zone_id: i64) -> u64 {
        self.zones
            .lock()
            .await
            .get(&zone_id)
            .map(|s| s.play_seq)
            .unwrap_or(0)
    }

    pub async fn play(&self, zone_id: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.state = PlayState::Playing;
        state.position_ms = 0;
        state.now_playing = Some(np);
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

        let data = now_playing_event_data(state);
        self.emit(PlaybackEvent {
            event: "started".into(),
            zone_id,
            data,
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
        let data = if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Stopped;
            state.last_seek_at = None;
            // Keep position_ms and now_playing so the UI shows where
            // playback left off and can resume from the same position.
            now_playing_event_data(state)
        } else {
            serde_json::json!({})
        };
        self.emit(PlaybackEvent {
            event: "stopped".into(),
            zone_id,
            data,
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
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.volume = volume.clamp(0.0, 1.0);
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
                if enabled {
                    // Build a fresh order around the currently playing track so
                    // the next advance goes to a different track.
                    state.shuffle_order = generate_shuffle_order(
                        state.queue_length.max(0) as usize,
                        state.queue_position.max(0) as usize,
                    );
                    state.shuffle_index = if state.shuffle_order.is_empty() {
                        -1
                    } else {
                        0
                    };
                } else {
                    state.shuffle_order.clear();
                    state.shuffle_index = -1;
                }
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
            if state.shuffle {
                let len = length.max(0) as usize;
                let pos = position.max(0) as usize;
                if len == 0 {
                    state.shuffle_order.clear();
                    state.shuffle_index = -1;
                } else if state.shuffle_order.len() != len {
                    // Queue length changed (tracks added/removed, or the order
                    // was lost across a restart — it is not persisted). Rebuild
                    // around the current track.
                    state.shuffle_order = generate_shuffle_order(len, pos);
                    state.shuffle_index = 0;
                } else if let Some(idx) = state.shuffle_order.iter().position(|&p| p == pos) {
                    // Sync the cursor to the track now playing so the next
                    // advance follows the order from here.
                    state.shuffle_index = idx as i64;
                } else {
                    // Position not in the order (shouldn't happen) — rebuild.
                    state.shuffle_order = generate_shuffle_order(len, pos);
                    state.shuffle_index = 0;
                }
            }
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
            state.now_playing = Some(np);
        }
        let data = zones
            .get(&zone_id)
            .map(now_playing_event_data)
            .unwrap_or_else(|| serde_json::json!({}));
        self.emit(PlaybackEvent {
            event: "track_changed".into(),
            zone_id,
            data,
        });
    }

    fn emit(&self, event: PlaybackEvent) {
        let _ = self.event_tx.send(event);
    }
}

/// Build the JSON payload for a now-playing WS event (`started` /
/// `track_changed` / `stopped`) from the full [`ZoneState`], so every one of
/// those events carries the same complete set of fields: the entire
/// [`NowPlaying`] (title, `track_id`, `format`, `sample_rate`, `bit_depth`, …)
/// plus `queue_position` / `queue_length` / `track_generation`. Each event used
/// to hand-write a different subset — `track_changed` (emitted on every gapless
/// advance) carried neither `track_id`, nor the quality fields, nor the queue
/// index — which forced the client to refetch the whole queue and delayed the
/// quality badge until a manual refresh (#1096, Benjithom).
fn now_playing_event_data(state: &ZoneState) -> serde_json::Value {
    let mut v = state
        .now_playing
        .as_ref()
        .and_then(|np| serde_json::to_value(np).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "queue_position".into(),
            serde_json::json!(state.queue_position),
        );
        obj.insert("queue_length".into(), serde_json::json!(state.queue_length));
        obj.insert(
            "track_generation".into(),
            serde_json::json!(state.track_generation),
        );
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_playing_event_data_carries_full_payload() {
        let mut state = ZoneState {
            zone_id: 1,
            state: PlayState::Playing,
            now_playing: Some(NowPlaying {
                track_id: Some(42),
                title: "Song".into(),
                bit_depth: Some(16),
                sample_rate: Some(44100),
                format: Some("flac".into()),
                ..Default::default()
            }),
            position_ms: 0,
            volume: 1.0,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            queue_position: 3,
            queue_length: 100,
            shuffle_order: vec![],
            shuffle_index: -1,
            track_generation: 7,
            play_seq: 0,
            last_seek_at: None,
            last_volume_set_at: None,
        };
        let v = now_playing_event_data(&state);
        // Full NowPlaying is serialised…
        assert_eq!(v["track_id"], 42);
        assert_eq!(v["bit_depth"], 16);
        assert_eq!(v["format"], "flac");
        // …plus the queue index/length and generation.
        assert_eq!(v["queue_position"], 3);
        assert_eq!(v["queue_length"], 100);
        assert_eq!(v["track_generation"], 7);

        // With no now_playing it still reports the queue fields, never panics.
        state.now_playing = None;
        let empty = now_playing_event_data(&state);
        assert_eq!(empty["queue_position"], 3);
        assert!(empty.get("track_id").is_none());
    }

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

    #[test]
    fn generate_shuffle_order_is_a_permutation_with_current_first() {
        let order = generate_shuffle_order(10, 4);
        assert_eq!(order.len(), 10);
        // Current track sits at index 0 so the first advance moves away from it.
        assert_eq!(order[0], 4);
        // Every index 0..10 appears exactly once (a true permutation).
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn generate_shuffle_order_edge_cases() {
        assert!(generate_shuffle_order(0, 0).is_empty());
        assert_eq!(generate_shuffle_order(1, 0), vec![0]);
        // current out of range is ignored (no panic), still a full permutation.
        let order = generate_shuffle_order(3, 99);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2]);
    }
}
