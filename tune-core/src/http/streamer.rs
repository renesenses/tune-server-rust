use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};
use tracing::info;

use crate::audio::wav::build_wav_header_with_duration;

pub const ICY_METAINT: usize = 16384;

#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub format: String,
    pub mime_type: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub file_size: Option<u64>,
    pub duration_ms: Option<u64>,
    pub seek_ms: Option<u64>,
}

impl StreamInfo {
    /// Calculate the expected WAV file size from audio parameters and duration.
    /// Returns `44 + data_bytes` (WAV header + raw PCM data).
    pub fn wav_content_length(&self) -> Option<u64> {
        let dur = self.duration_ms?;
        if self.sample_rate == 0 || self.channels == 0 || self.bit_depth == 0 {
            return None;
        }
        let bytes_per_sample = self.bit_depth as u64 / 8;
        let data_bytes =
            dur * self.sample_rate as u64 * self.channels as u64 * bytes_per_sample / 1000;
        Some(44 + data_bytes)
    }
}

pub struct StreamSession {
    pub id: String,
    pub info: StreamInfo,
    pub tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Keeps the channel open until the session is removed, even after the
    /// decoder drops its tx. Without this, the HTTP stream ends as soon as
    /// the decoder finishes, before ASIO/WASAPI has consumed all buffered data.
    _keep_alive_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    pub file_path: Mutex<Option<String>>,
    pub proxy_url: Mutex<Option<String>>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub track_album: Option<String>,
    pub cover_url: Option<String>,
    pub bit_perfect: bool,
    pub is_radio: bool,
    pub wav_header_included: std::sync::atomic::AtomicBool,
    pub created_at: Instant,
    pub bytes_sent: std::sync::atomic::AtomicU64,
    pub first_request: std::sync::Arc<tokio::sync::Notify>,
    pub data_ready: std::sync::Arc<tokio::sync::Notify>,
}

impl StreamSession {
    pub fn new(id: String, info: StreamInfo, bit_perfect: bool, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        let keep_alive = tx.clone();
        Self {
            id,
            info,
            tx: Mutex::new(Some(tx)),
            _keep_alive_tx: Mutex::new(Some(keep_alive)),
            rx: Mutex::new(rx),
            file_path: Mutex::new(None),
            proxy_url: Mutex::new(None),
            track_title: None,
            track_artist: None,
            track_album: None,
            cover_url: None,
            bit_perfect,
            is_radio: false,
            wav_header_included: std::sync::atomic::AtomicBool::new(false),
            created_at: Instant::now(),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            first_request: std::sync::Arc::new(tokio::sync::Notify::new()),
            data_ready: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub async fn recv_chunk(&self) -> Option<Vec<u8>> {
        self.rx.lock().await.recv().await
    }

    pub async fn close_sender(&self) {
        self.tx.lock().await.take();
        self._keep_alive_tx.lock().await.take();
    }
}

/// Type alias for the shared sessions map, used by both core and server.
pub type SharedSessions = Arc<Mutex<HashMap<String, Arc<StreamSession>>>>;

pub struct AudioStreamer {
    sessions: Arc<Mutex<HashMap<String, Arc<StreamSession>>>>,
    port: u16,
}

impl AudioStreamer {
    pub fn new(port: u16) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            port,
        }
    }

    pub async fn create_session(
        &self,
        info: StreamInfo,
        bit_perfect: bool,
        buffer_size: usize,
    ) -> (
        String,
        mpsc::Sender<Vec<u8>>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let session = StreamSession::new(id.clone(), info, bit_perfect, buffer_size);
        let tx = session
            .tx
            .lock()
            .await
            .take()
            .expect("freshly created session has tx");
        let data_ready = session.data_ready.clone();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "stream_session_created");
        (id, tx, data_ready)
    }

    pub async fn wait_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        let notify = {
            let sessions = self.sessions.lock().await;
            sessions.get(stream_id).map(|s| s.data_ready.clone())
        };
        let Some(notify) = notify else {
            return false;
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            notify.notified(),
        )
        .await
        .is_ok()
    }

    pub async fn create_file_session(
        &self,
        info: StreamInfo,
        file_path: String,
        bit_perfect: bool,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let session = StreamSession::new(id.clone(), info, bit_perfect, 64);
        *session.file_path.lock().await = Some(file_path);
        // File is already written to disk — signal data_ready immediately so
        // gapless pre-buffer logic (poller::prepare_gapless → wait_stream_data_ready)
        // does not block for its full 5-second timeout waiting for data that will
        // never arrive via the mpsc channel.
        session.data_ready.notify_one();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "file_session_created");
        id
    }

    /// Create a streaming session for a decoded radio stream (infinite,
    /// exempt from GC).  Same as `create_session` but sets `is_radio = true`
    /// so the stream handler skips coalescing and the GC retains the session.
    pub async fn create_radio_session(
        &self,
        info: StreamInfo,
        buffer_size: usize,
    ) -> (
        String,
        mpsc::Sender<Vec<u8>>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = StreamSession::new(id.clone(), info, false, buffer_size);
        session.is_radio = true;
        let tx = session
            .tx
            .lock()
            .await
            .take()
            .expect("freshly created session has tx");
        let data_ready = session.data_ready.clone();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "radio_stream_session_created");
        (id, tx, data_ready)
    }

    pub async fn create_proxy_session(
        &self,
        info: StreamInfo,
        upstream_url: String,
        is_radio: bool,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = StreamSession::new(id.clone(), info, false, 128);
        session.is_radio = is_radio;
        *session.proxy_url.lock().await = Some(upstream_url);
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, is_radio, "proxy_session_created");
        id
    }

    /// Check if a session is a proxy session (direct CDN URL forwarding)
    /// or a file session — both support HTTP Range-based seeking.
    /// Decoded/transcoded WAV sessions (mpsc channel) do NOT support Range seeking.
    pub async fn is_seekable_session(&self, stream_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(stream_id) {
            let has_proxy = session.proxy_url.lock().await.is_some();
            let has_file = session.file_path.lock().await.is_some();
            has_proxy || has_file
        } else {
            false
        }
    }

    pub async fn remove_session(&self, stream_id: &str) {
        // Close the channel senders BEFORE removing — this ensures the
        // radio decode thread's tx.send() fails promptly, stopping
        // the icecast download. Without this, radio streams continue
        // playing as "ghosts" after stop.
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(stream_id) {
                session.close_sender().await;
            }
        }
        let removed = self.sessions.lock().await.remove(stream_id);
        // Clean up temp transcode files created by the pre-transcode pipeline.
        // Only delete files under the system temp dir with the tune-transcode prefix
        // to avoid accidentally removing actual music files.
        if let Some(session) = removed {
            let fp = session.file_path.lock().await;
            if let Some(ref path) = *fp {
                if is_temp_transcode_file(path) {
                    if let Err(e) = std::fs::remove_file(path) {
                        info!(stream_id, path, error = %e, "temp_transcode_file_cleanup_failed");
                    } else {
                        info!(stream_id, path, "temp_transcode_file_cleaned_up");
                    }
                }
            }
        }
        info!(stream_id, "stream_session_removed");
    }

    pub fn get_stream_url(&self, stream_id: &str, server_ip: &str, ext: &str) -> String {
        format!("http://{server_ip}:{}/stream/{stream_id}.{ext}", self.port)
    }

    pub fn sessions_state(&self) -> Arc<Mutex<HashMap<String, Arc<StreamSession>>>> {
        self.sessions.clone()
    }

    pub async fn stream_bytes_sent(&self, stream_id: &str) -> Option<u64> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(stream_id)
            .map(|s| s.bytes_sent.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub async fn cleanup_stale_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        // Collect temp files to clean up from stale sessions
        let mut temp_files_to_remove: Vec<String> = Vec::new();
        // 30 minutes for streaming sessions — Hi-Res tracks can be large
        // and DLNA renderers may buffer slowly.  Orphaned sessions from
        // gapless prep or interrupted playback are cleaned up sooner by
        // the orchestrator; this GC is the safety net.
        sessions.retain(|id, s| {
            if s.is_radio {
                return true;
            }
            let age = s.created_at.elapsed();
            if age > std::time::Duration::from_secs(1800) {
                // Check for temp transcode file to clean up.
                // We can't .await inside retain, so use try_lock.
                if let Ok(fp) = s.file_path.try_lock() {
                    if let Some(ref path) = *fp {
                        if is_temp_transcode_file(path) {
                            temp_files_to_remove.push(path.clone());
                        }
                    }
                }
                info!(stream_id = %id, age_secs = age.as_secs(), "stale_session_removed");
                false
            } else {
                true
            }
        });
        let after = sessions.len();
        drop(sessions);
        // Clean up temp files outside the sessions lock
        for path in &temp_files_to_remove {
            if let Err(e) = std::fs::remove_file(path) {
                info!(path, error = %e, "stale_temp_transcode_file_cleanup_failed");
            } else {
                info!(path, "stale_temp_transcode_file_cleaned_up");
            }
        }
        before - after
    }
}

/// Remove leftover temp transcode files from /tmp on startup.
/// Called once when the server starts to clean up files from a previous
/// crash or unclean shutdown.
pub fn cleanup_leftover_transcode_files() {
    let tmp_dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&tmp_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut count = 0;
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("tune-transcode-")
                || name.starts_with("tune-aac-transcode-")
                || name.starts_with("tune-dash-transcode-")
            {
                if std::fs::remove_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
    }
    if count > 0 {
        info!(count, "leftover_transcode_files_cleaned_up");
    }
}

// ─── Helpers (framework-agnostic) ───────────────────────────────

pub fn extract_stream_id(raw: &str) -> &str {
    raw.split('.').next().unwrap_or(raw)
}

pub fn build_wav_header(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    duration_ms: Option<u64>,
) -> [u8; 44] {
    build_wav_header_with_duration(channels, sample_rate, bit_depth, duration_ms)
}

/// Check if a file path is a temporary transcode file created by the
/// pre-transcode pipeline.  Only these files should be auto-deleted
/// when a session is removed — never actual music files.
///
/// Patterns:
/// - `tune-transcode-{uuid}.{ext}` — local file pre-transcode (FLAC/WAV target)
/// - `tune-aac-transcode-{uuid}.flac` — Tidal AAC→FLAC pre-transcode
/// - `tune-dash-transcode-{uuid}.flac` — Tidal DASH fMP4→FLAC pre-transcode
fn is_temp_transcode_file(path: &str) -> bool {
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    file_name.starts_with("tune-transcode-")
        || file_name.starts_with("tune-aac-transcode-")
        || file_name.starts_with("tune-dash-transcode-")
}

pub fn build_icy_metadata(
    artist: Option<&str>,
    title: Option<&str>,
    cover_url: Option<&str>,
) -> Vec<u8> {
    let mut parts = Vec::new();
    let stream_title = match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{a} - {t}")),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
    };
    if let Some(st) = stream_title {
        parts.push(format!("StreamTitle='{st}';"));
    }
    if let Some(url) = cover_url {
        parts.push(format!("StreamUrl='{url}';"));
    }
    if parts.is_empty() {
        return vec![0u8];
    }
    let mut payload = parts.join("").into_bytes();
    let pad = (16 - payload.len() % 16) % 16;
    payload.resize(payload.len() + pad, 0);
    let len_byte = (payload.len() / 16).min(255) as u8;
    let mut block = vec![len_byte];
    block.extend_from_slice(&payload[..len_byte as usize * 16]);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_remove_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let (id, _tx, _data_ready) = streamer.create_session(info, false, 128).await;
        assert!(!id.is_empty());
        streamer.remove_session(&id).await;
    }

    #[tokio::test]
    async fn file_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            file_size: Some(50_000_000),
            duration_ms: None,
            ..Default::default()
        };
        let id = streamer
            .create_file_session(info, "/music/test.flac".into(), true)
            .await;
        let url = streamer.get_stream_url(&id, "192.168.1.18", "flac");
        assert!(url.contains(".flac"));
        streamer.remove_session(&id).await;
    }

    #[test]
    fn icy_metadata_block() {
        let block = build_icy_metadata(Some("Artist"), Some("Title"), None);
        assert!(block.len() > 1);
        let len_byte = block[0] as usize;
        assert_eq!(block.len(), 1 + len_byte * 16);
        let payload = std::str::from_utf8(&block[1..]).unwrap();
        assert!(payload.contains("StreamTitle='Artist - Title'"));
    }

    #[test]
    fn icy_metadata_empty() {
        let block = build_icy_metadata(None, None, None);
        assert_eq!(block, vec![0u8]);
    }

    #[test]
    fn icy_metadata_with_cover() {
        let block = build_icy_metadata(Some("A"), Some("T"), Some("http://example.com/cover.jpg"));
        let payload = String::from_utf8_lossy(&block[1..]);
        assert!(payload.contains("StreamUrl='http://example.com/cover.jpg'"));
    }

    #[tokio::test]
    async fn proxy_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let id = streamer
            .create_proxy_session(info, "https://cdn.tidal.com/track.flac".into(), false)
            .await;
        assert!(!id.is_empty());
        streamer.remove_session(&id).await;
    }

    #[test]
    fn stream_id_extraction() {
        assert_eq!(extract_stream_id("abc123.flac"), "abc123");
        assert_eq!(extract_stream_id("abc123"), "abc123");
    }

    #[test]
    fn wav_content_length_known_duration() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: Some(180_000),
            ..Default::default()
        };
        // 180s * 44100 * 2ch * 2bytes + 44 header
        let expected = 180 * 44100 * 2 * 2 + 44;
        assert_eq!(info.wav_content_length(), Some(expected));
    }

    #[test]
    fn wav_content_length_no_duration() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        assert_eq!(info.wav_content_length(), None);
    }

    #[test]
    fn wav_content_length_hires() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            file_size: None,
            duration_ms: Some(256_487),
            ..Default::default()
        };
        let expected = 256_487u64 * 96000 * 2 * 3 / 1000 + 44;
        assert_eq!(info.wav_content_length(), Some(expected));
    }
}
