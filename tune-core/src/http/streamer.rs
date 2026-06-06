use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};
use tracing::info;

use crate::audio::wav::build_wav_header_with_duration;

pub const ICY_METAINT: usize = 16384;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub format: String,
    pub mime_type: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub file_size: Option<u64>,
    pub duration_ms: Option<u64>,
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
    pub tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    pub file_path: Mutex<Option<String>>,
    pub proxy_url: Mutex<Option<String>>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub track_album: Option<String>,
    pub cover_url: Option<String>,
    pub bit_perfect: bool,
    pub is_radio: bool,
    pub created_at: Instant,
    pub bytes_sent: std::sync::atomic::AtomicU64,
    pub first_request: std::sync::Arc<tokio::sync::Notify>,
}

impl StreamSession {
    pub fn new(id: String, info: StreamInfo, bit_perfect: bool, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        Self {
            id,
            info,
            tx,
            rx: Mutex::new(rx),
            file_path: Mutex::new(None),
            proxy_url: Mutex::new(None),
            track_title: None,
            track_artist: None,
            track_album: None,
            cover_url: None,
            bit_perfect,
            is_radio: false,
            created_at: Instant::now(),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            first_request: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub async fn recv_chunk(&self) -> Option<Vec<u8>> {
        self.rx.lock().await.recv().await
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
    ) -> (String, mpsc::Sender<Vec<u8>>) {
        let id = uuid::Uuid::new_v4().to_string();
        let session = StreamSession::new(id.clone(), info, bit_perfect, buffer_size);
        let tx = session.tx.clone();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "stream_session_created");
        (id, tx)
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
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "file_session_created");
        id
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

    pub async fn remove_session(&self, stream_id: &str) {
        self.sessions.lock().await.remove(stream_id);
        info!(stream_id, "stream_session_removed");
    }

    pub fn get_stream_url(&self, stream_id: &str, server_ip: &str, ext: &str) -> String {
        format!("http://{server_ip}:{}/stream/{stream_id}.{ext}", self.port)
    }

    pub fn sessions_state(&self) -> Arc<Mutex<HashMap<String, Arc<StreamSession>>>> {
        self.sessions.clone()
    }

    pub async fn get_stream_ready_notify(
        &self,
        stream_id: &str,
    ) -> Option<std::sync::Arc<tokio::sync::Notify>> {
        let sessions = self.sessions.lock().await;
        sessions.get(stream_id).map(|s| s.first_request.clone())
    }

    pub async fn wait_first_request(&self, stream_id: &str, timeout_ms: u64) -> bool {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(stream_id).cloned()
        };
        let Some(session) = session else {
            return false;
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            session.first_request.notified(),
        )
        .await
        .is_ok()
    }

    pub async fn cleanup_stale_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        // 5 minutes is generous for any legitimate stream.  Orphaned sessions
        // from gapless prep or interrupted playback are cleaned up sooner by
        // the orchestrator; this GC is the safety net.
        sessions.retain(|id, s| {
            let age = s.created_at.elapsed();
            if age > std::time::Duration::from_secs(300) {
                info!(stream_id = %id, age_secs = age.as_secs(), "stale_session_removed");
                false
            } else {
                true
            }
        });
        before - sessions.len()
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
        };
        let (id, _tx) = streamer.create_session(info, false, 128).await;
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
        };
        let expected = 256_487u64 * 96000 * 2 * 3 / 1000 + 44;
        assert_eq!(info.wav_content_length(), Some(expected));
    }
}
