//! Streaming pre-buffering engine.
//!
//! Downloads and decodes the next streaming track in the queue while the
//! current track is playing, eliminating the 3-second gap between tracks
//! from services like Tidal, Qobuz, and Deezer.
//!
//! The engine supports three modes:
//! - `Off`: no prefetching
//! - `Buffer30s`: download/decode the first 30 seconds of the next track
//! - `Full`: download/decode the entire next track
//!
//! The prefetched PCM data is stored in memory and consumed when the
//! orchestrator transitions to the next track.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

/// Maximum PCM buffer size (~100 MB) — safety valve to prevent OOM on
/// very long hi-res tracks in Full mode.
const MAX_BUFFER_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrefetchMode {
    Off,
    #[serde(rename = "30s")]
    Buffer30s,
    Full,
}

impl PrefetchMode {
    pub fn from_str_setting(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "off" | "disabled" | "false" | "0" => PrefetchMode::Off,
            "full" | "true" | "1" => PrefetchMode::Full,
            _ => PrefetchMode::Buffer30s, // "30s" or any other value
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PrefetchMode::Off => "off",
            PrefetchMode::Buffer30s => "30s",
            PrefetchMode::Full => "full",
        }
    }
}

/// Decoded audio data ready to be served immediately.
#[derive(Debug, Clone)]
pub struct PrefetchedTrack {
    pub source: String,
    pub source_id: String,
    pub stream_id: Option<String>,
    pub pcm_data: Vec<u8>,
    pub format: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub duration_ms: u64,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub mime_type: String,
}

/// Thread-safe prefetch engine shared across async tasks.
pub struct PrefetchEngine {
    buffer: Mutex<Option<PrefetchedTrack>>,
    /// Cancellation token: set to true to abort an in-progress prefetch.
    cancel: Mutex<bool>,
}

impl Default for PrefetchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PrefetchEngine {
    pub fn new() -> Self {
        Self {
            buffer: Mutex::new(None),
            cancel: Mutex::new(false),
        }
    }

    /// Read the current prefetch mode from settings.
    pub fn read_mode(db: &Arc<dyn DbBackend>) -> PrefetchMode {
        SettingsRepo::with_backend(db.clone())
            .get("prefetch_mode")
            .ok()
            .flatten()
            .map(|s| PrefetchMode::from_str_setting(&s))
            .unwrap_or(PrefetchMode::Buffer30s)
    }

    /// Prefetch the next track in the queue for the given zone.
    ///
    /// This method:
    /// 1. Reads the queue and finds the next streaming track
    /// 2. Downloads the audio from the streaming service
    /// 3. Decodes it to PCM
    /// 4. Stores the result in the internal buffer
    ///
    /// Only prefetches streaming sources (tidal, qobuz, deezer, youtube).
    /// Local files are served directly from disk — no benefit to prefetching.
    pub async fn prefetch_next(
        &self,
        db: Arc<dyn DbBackend>,
        services: Arc<Mutex<ServiceRegistry>>,
        playback: Arc<PlaybackManager>,
        zone_id: i64,
    ) {
        let mode = Self::read_mode(&db);
        if mode == PrefetchMode::Off {
            debug!(zone_id, "prefetch_disabled");
            return;
        }

        // Signal any in-progress prefetch to stop, then reset the flag.
        {
            let mut cancel = self.cancel.lock().await;
            *cancel = true;
        }
        // Small yield to let any running prefetch task notice the cancel.
        tokio::task::yield_now().await;
        {
            let mut cancel = self.cancel.lock().await;
            *cancel = false;
        }

        // Find the next track in the queue
        let zone_state = playback.get_state(zone_id).await;
        let current_pos = zone_state.queue_position;
        let queue_len = zone_state.queue_length;

        if queue_len == 0 {
            debug!(zone_id, "prefetch_no_queue");
            return;
        }

        // Compute next position (respecting repeat/shuffle is not needed
        // for prefetch — the poller/orchestrator handle that; we just
        // prefetch position+1 as a best guess)
        let next_pos = current_pos + 1;
        if next_pos >= queue_len {
            debug!(zone_id, "prefetch_end_of_queue");
            return;
        }

        let queue_repo = PlayQueueRepo::with_backend(db.clone());

        // Try streaming queue first (Tidal, Qobuz, etc.)
        let streaming_queue = queue_repo
            .get_streaming_queue(zone_id)
            .ok()
            .unwrap_or_default();
        if let Some(item) = streaming_queue.get(next_pos as usize) {
            let source = item["source"].as_str().unwrap_or("tidal").to_string();
            let source_id = item["source_id"].as_str().unwrap_or("").to_string();

            if !is_streaming_source(&source) {
                debug!(zone_id, source = %source, "prefetch_skip_non_streaming");
                return;
            }

            if source_id.is_empty() {
                debug!(zone_id, "prefetch_skip_empty_source_id");
                return;
            }

            let title = item["title"].as_str().map(String::from);
            let artist = item["artist_name"].as_str().map(String::from);
            let album = item["album_title"].as_str().map(String::from);
            let cover = item["cover_path"].as_str().map(String::from);
            let duration_ms = item["duration_ms"].as_u64().unwrap_or(0);

            info!(
                zone_id,
                source = %source,
                source_id = %source_id,
                title = ?title,
                next_pos,
                mode = mode.as_str(),
                "prefetch_starting"
            );

            self.do_prefetch(
                db,
                services,
                mode,
                source,
                source_id,
                title,
                artist,
                album,
                cover,
                duration_ms,
            )
            .await;
        } else {
            // Local queue — no prefetch needed for local files
            debug!(zone_id, next_pos, "prefetch_skip_local_track");
        }
    }

    /// Internal: download, decode, and buffer a streaming track.
    async fn do_prefetch(
        &self,
        _db: Arc<dyn DbBackend>,
        services: Arc<Mutex<ServiceRegistry>>,
        mode: PrefetchMode,
        source: String,
        source_id: String,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        cover_url: Option<String>,
        duration_ms: u64,
    ) {
        // Resolve the stream URL from the service
        let stream_data = {
            let registry = services.lock().await;
            let svc = match registry.get(&source) {
                Some(s) => s,
                None => {
                    warn!(source = %source, "prefetch_unknown_service");
                    return;
                }
            };
            let mut svc = svc.lock().await;
            match svc.get_track_url(&source_id, None).await {
                Ok(data) => data,
                Err(e) => {
                    // Try refresh once on auth errors
                    let msg = e.to_string();
                    if msg.contains("401") || msg.contains("403") {
                        if svc.refresh_if_needed().await.unwrap_or(false) {
                            match svc.get_track_url(&source_id, None).await {
                                Ok(data) => data,
                                Err(e2) => {
                                    warn!(error = %e2, "prefetch_get_url_failed_after_refresh");
                                    return;
                                }
                            }
                        } else {
                            warn!(error = %e, "prefetch_get_url_auth_failed");
                            return;
                        }
                    } else {
                        warn!(error = %e, "prefetch_get_url_failed");
                        return;
                    }
                }
            }
        };

        // Fetch track metadata if not provided
        let (track_title, track_artist, track_album, track_cover, actual_duration) =
            if title.is_some() {
                (
                    title.unwrap_or_default(),
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                )
            } else {
                let registry = services.lock().await;
                if let Some(svc) = registry.get(&source) {
                    let svc = svc.lock().await;
                    match svc.get_track(&source_id).await {
                        Ok(track) => (
                            track.title,
                            Some(track.artist),
                            track.album,
                            track.cover_path,
                            track.duration_ms,
                        ),
                        Err(_) => ("Unknown".into(), None, None, None, duration_ms),
                    }
                } else {
                    ("Unknown".into(), None, None, None, duration_ms)
                }
            };

        let upstream_url = stream_data.url.clone();
        let sr = stream_data.quality.sample_rate;
        let bd = stream_data.quality.bit_depth;
        let codec = stream_data.quality.codec.to_lowercase();

        // Calculate max bytes for Buffer30s mode
        // 30 seconds of PCM at sr * channels * (bd/8)
        let bytes_per_second = sr as usize * 2 * (bd.max(16) as usize / 8);
        let max_bytes = match mode {
            PrefetchMode::Buffer30s => (bytes_per_second * 30).min(MAX_BUFFER_BYTES),
            PrefetchMode::Full => MAX_BUFFER_BYTES,
            PrefetchMode::Off => return,
        };

        let source_clone = source.clone();
        let source_id_clone = source_id.clone();

        // Check cancel before starting the download
        if *self.cancel.lock().await {
            debug!("prefetch_cancelled_before_download");
            return;
        }

        // Download and decode in a blocking task
        let is_dash_file = upstream_url.starts_with("file://");
        let decode_result = tokio::task::spawn_blocking(move || {
            // Download to temp file
            let tmp_path = if is_dash_file {
                upstream_url
                    .strip_prefix("file://")
                    .unwrap_or(&upstream_url)
                    .to_string()
            } else {
                let tmp_path = std::env::temp_dir()
                    .join(format!("tune-prefetch-{}.{}", uuid::Uuid::new_v4(), codec))
                    .to_string_lossy()
                    .to_string();

                let resp = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(120))
                    .build()
                    .and_then(|c| c.get(&upstream_url).send());

                match resp {
                    Ok(mut r) if r.status().is_success() => {
                        let mut file = match std::fs::File::create(&tmp_path) {
                            Ok(f) => f,
                            Err(e) => return Err(format!("prefetch tmp create: {e}")),
                        };
                        match std::io::copy(&mut r, &mut file) {
                            Ok(bytes) => {
                                debug!(bytes, "prefetch_download_complete");
                            }
                            Err(e) => {
                                let _ = std::fs::remove_file(&tmp_path);
                                return Err(format!("prefetch download: {e}"));
                            }
                        }
                    }
                    Ok(r) => {
                        return Err(format!("prefetch HTTP {}", r.status()));
                    }
                    Err(e) => {
                        return Err(format!("prefetch fetch: {e}"));
                    }
                }
                tmp_path
            };

            // Decode to PCM
            let result =
                crate::audio::decode::decode_to_pcm(&tmp_path, Some(sr), Some(2), 0.0, 0.0);

            // Clean up temp file (but not DASH files — they're managed elsewhere)
            if !is_dash_file {
                let _ = std::fs::remove_file(&tmp_path);
            }

            match result {
                Ok(decoded) => {
                    let mut pcm = decoded.pcm_bytes();

                    // Truncate to max_bytes for Buffer30s mode
                    if pcm.len() > max_bytes {
                        pcm.truncate(max_bytes);
                    }

                    Ok((
                        pcm,
                        decoded.sample_rate,
                        decoded.bit_depth,
                        decoded.channels,
                    ))
                }
                Err(e) => Err(format!("prefetch decode: {e}")),
            }
        })
        .await;

        // Check cancel after download
        if *self.cancel.lock().await {
            debug!("prefetch_cancelled_after_download");
            return;
        }

        match decode_result {
            Ok(Ok((pcm_data, actual_sr, actual_bd, actual_ch))) => {
                let buffer_secs = if actual_sr > 0 && actual_ch > 0 && actual_bd > 0 {
                    pcm_data.len() as f64
                        / (actual_sr as f64 * actual_ch as f64 * (actual_bd as f64 / 8.0))
                } else {
                    0.0
                };

                info!(
                    source = %source_clone,
                    source_id = %source_id_clone,
                    title = %track_title,
                    buffer_bytes = pcm_data.len(),
                    buffer_secs = format!("{:.1}", buffer_secs),
                    sample_rate = actual_sr,
                    bit_depth = actual_bd,
                    channels = actual_ch,
                    "prefetch_complete"
                );

                let prefetched = PrefetchedTrack {
                    source: source_clone,
                    source_id: source_id_clone,
                    stream_id: None,
                    pcm_data,
                    format: "wav".into(),
                    sample_rate: actual_sr,
                    bit_depth: actual_bd as u16,
                    channels: actual_ch as u16,
                    duration_ms: actual_duration,
                    title: track_title,
                    artist: track_artist,
                    album: track_album,
                    cover_url: track_cover,
                    mime_type: "audio/wav".into(),
                };

                *self.buffer.lock().await = Some(prefetched);
            }
            Ok(Err(e)) => {
                warn!(error = %e, "prefetch_failed");
            }
            Err(e) => {
                warn!(error = %e, "prefetch_task_panic");
            }
        }
    }

    /// Consume the prefetched track if it matches the requested source/source_id.
    ///
    /// Returns `Some(PrefetchedTrack)` and clears the buffer if it matches,
    /// `None` otherwise (the buffer is left intact for future use).
    pub async fn take_prefetched(&self, source: &str, source_id: &str) -> Option<PrefetchedTrack> {
        let mut buf = self.buffer.lock().await;
        if let Some(ref prefetched) = *buf {
            if prefetched.source == source && prefetched.source_id == source_id {
                info!(
                    source = %source,
                    source_id = %source_id,
                    buffer_bytes = prefetched.pcm_data.len(),
                    "prefetch_consumed"
                );
                return buf.take();
            }
            debug!(
                expected_source = %source,
                expected_id = %source_id,
                buffered_source = %prefetched.source,
                buffered_id = %prefetched.source_id,
                "prefetch_mismatch"
            );
        }
        None
    }

    /// Check if a prefetched track is available (without consuming it).
    pub async fn has_prefetched(&self, source: &str, source_id: &str) -> bool {
        let buf = self.buffer.lock().await;
        buf.as_ref()
            .is_some_and(|p| p.source == source && p.source_id == source_id)
    }

    /// Clear the prefetch buffer. Called when the queue changes or when
    /// the user skips/stops — the buffered track is no longer relevant.
    pub async fn clear(&self) {
        let mut cancel = self.cancel.lock().await;
        *cancel = true;
        drop(cancel);

        let mut buf = self.buffer.lock().await;
        if buf.is_some() {
            info!("prefetch_buffer_cleared");
        }
        *buf = None;
    }

    /// Return diagnostic info about the current prefetch state.
    pub async fn status(&self) -> serde_json::Value {
        let buf = self.buffer.lock().await;
        match buf.as_ref() {
            Some(p) => serde_json::json!({
                "buffered": true,
                "source": p.source,
                "source_id": p.source_id,
                "title": p.title,
                "artist": p.artist,
                "buffer_bytes": p.pcm_data.len(),
                "sample_rate": p.sample_rate,
                "bit_depth": p.bit_depth,
                "channels": p.channels,
                "duration_ms": p.duration_ms,
            }),
            None => serde_json::json!({
                "buffered": false,
            }),
        }
    }
}

/// Returns true if the source is a streaming service (not local files).
fn is_streaming_source(source: &str) -> bool {
    matches!(
        source,
        "tidal" | "qobuz" | "deezer" | "youtube" | "spotify" | "amazon"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_mode_from_str() {
        assert_eq!(PrefetchMode::from_str_setting("off"), PrefetchMode::Off);
        assert_eq!(
            PrefetchMode::from_str_setting("disabled"),
            PrefetchMode::Off
        );
        assert_eq!(PrefetchMode::from_str_setting("false"), PrefetchMode::Off);
        assert_eq!(PrefetchMode::from_str_setting("0"), PrefetchMode::Off);
        assert_eq!(PrefetchMode::from_str_setting("full"), PrefetchMode::Full);
        assert_eq!(PrefetchMode::from_str_setting("true"), PrefetchMode::Full);
        assert_eq!(PrefetchMode::from_str_setting("1"), PrefetchMode::Full);
        assert_eq!(
            PrefetchMode::from_str_setting("30s"),
            PrefetchMode::Buffer30s
        );
        assert_eq!(
            PrefetchMode::from_str_setting("anything"),
            PrefetchMode::Buffer30s
        );
    }

    #[test]
    fn prefetch_mode_as_str() {
        assert_eq!(PrefetchMode::Off.as_str(), "off");
        assert_eq!(PrefetchMode::Buffer30s.as_str(), "30s");
        assert_eq!(PrefetchMode::Full.as_str(), "full");
    }

    #[test]
    fn is_streaming() {
        assert!(is_streaming_source("tidal"));
        assert!(is_streaming_source("qobuz"));
        assert!(is_streaming_source("deezer"));
        assert!(is_streaming_source("youtube"));
        assert!(is_streaming_source("spotify"));
        assert!(is_streaming_source("amazon"));
        assert!(!is_streaming_source("local"));
        assert!(!is_streaming_source("radio"));
        assert!(!is_streaming_source("podcast"));
    }

    #[tokio::test]
    async fn engine_new_and_clear() {
        let engine = PrefetchEngine::new();
        assert!(engine.take_prefetched("tidal", "123").await.is_none());

        // Clear on empty buffer should not panic
        engine.clear().await;
    }

    #[tokio::test]
    async fn take_prefetched_match_and_mismatch() {
        let engine = PrefetchEngine::new();

        // Insert a prefetched track manually
        let track = PrefetchedTrack {
            source: "tidal".into(),
            source_id: "abc123".into(),
            stream_id: None,
            pcm_data: vec![0u8; 1024],
            format: "wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            duration_ms: 180_000,
            title: "Test Track".into(),
            artist: Some("Test Artist".into()),
            album: Some("Test Album".into()),
            cover_url: None,
            mime_type: "audio/wav".into(),
        };
        *engine.buffer.lock().await = Some(track);

        // Mismatch: different source_id
        assert!(engine.take_prefetched("tidal", "xyz789").await.is_none());
        // Buffer should still be there
        assert!(engine.has_prefetched("tidal", "abc123").await);

        // Mismatch: different source
        assert!(engine.take_prefetched("qobuz", "abc123").await.is_none());

        // Match: consumes the buffer
        let taken = engine.take_prefetched("tidal", "abc123").await;
        assert!(taken.is_some());
        let t = taken.unwrap();
        assert_eq!(t.source, "tidal");
        assert_eq!(t.source_id, "abc123");
        assert_eq!(t.pcm_data.len(), 1024);

        // Buffer should be empty now
        assert!(!engine.has_prefetched("tidal", "abc123").await);
    }

    #[tokio::test]
    async fn status_empty_and_buffered() {
        let engine = PrefetchEngine::new();

        let status = engine.status().await;
        assert_eq!(status["buffered"], false);

        *engine.buffer.lock().await = Some(PrefetchedTrack {
            source: "qobuz".into(),
            source_id: "42".into(),
            stream_id: None,
            pcm_data: vec![0u8; 2048],
            format: "wav".into(),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            duration_ms: 300_000,
            title: "Buffered Track".into(),
            artist: None,
            album: None,
            cover_url: None,
            mime_type: "audio/wav".into(),
        });

        let status = engine.status().await;
        assert_eq!(status["buffered"], true);
        assert_eq!(status["source"], "qobuz");
        assert_eq!(status["source_id"], "42");
        assert_eq!(status["buffer_bytes"], 2048);
        assert_eq!(status["sample_rate"], 96000);
    }
}
