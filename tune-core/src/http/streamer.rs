use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::audio::wav::build_wav_header_with_duration;

const ICY_METAINT: usize = 16384;

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
        }
    }

    async fn recv_chunk(&self) -> Option<Vec<u8>> {
        self.rx.lock().await.recv().await
    }
}

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

    pub async fn cleanup_stale_sessions(&self) -> usize {
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        sessions.retain(|id, s| {
            let age = s.created_at.elapsed();
            if age > std::time::Duration::from_secs(1800) {
                info!(stream_id = %id, age_secs = age.as_secs(), "stale_session_removed");
                false
            } else {
                true
            }
        });
        before - sessions.len()
    }
}

// ─── Axum handlers ──────────────────────────────────────────────

type SharedSessions = Arc<Mutex<HashMap<String, Arc<StreamSession>>>>;

fn extract_stream_id(raw: &str) -> &str {
    raw.split('.').next().unwrap_or(raw)
}

pub async fn handle_head(
    Path(raw_id): Path<String>,
    State(sessions): State<SharedSessions>,
) -> Response {
    let stream_id = extract_stream_id(&raw_id);
    let sessions = sessions.lock().await;

    let Some(session) = sessions.get(stream_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    info!(
        stream_id,
        format = %session.info.format,
        file_size = ?session.info.file_size,
        "stream_head_request"
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&session.info.mime_type).unwrap(),
    );
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    if let Some(size) = session.info.file_size {
        headers.insert("Content-Length", HeaderValue::from(size));
    }

    (StatusCode::OK, headers).into_response()
}

pub async fn handle_stream(
    Path(raw_id): Path<String>,
    State(sessions): State<SharedSessions>,
    req_headers: HeaderMap,
) -> Response {
    let stream_id = extract_stream_id(&raw_id);
    let session = {
        let sessions = sessions.lock().await;
        sessions.get(stream_id).cloned()
    };

    let Some(session) = session else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let range_hdr = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    let user_agent = req_headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    info!(
        stream_id,
        range = range_hdr,
        agent = user_agent,
        format = %session.info.format,
        "stream_request"
    );

    // File serving with Range support
    let file_path = session.file_path.lock().await.clone();
    if let Some(ref path) = file_path {
        return serve_file(path, &session.info, &req_headers).await;
    }

    // Proxy mode
    let proxy_url = session.proxy_url.lock().await.clone();
    if let Some(ref url) = proxy_url {
        return proxy_stream(url, &session.info, session.is_radio, &req_headers).await;
    }

    // Chunked streaming mode
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&session.info.mime_type).unwrap(),
    );
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));

    // When we know the WAV content length, send it so DLNA renderers
    // (DMP-A6/A8) don't need to probe the stream end with seek requests.
    let is_wav = session.info.format == "wav";
    let wav_length = if is_wav {
        session.info.wav_content_length()
    } else {
        None
    };
    if let Some(len) = wav_length {
        headers.insert("Content-Length", HeaderValue::from(len));
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    }

    let wants_icy = req_headers
        .get("Icy-MetaData")
        .and_then(|v| v.to_str().ok())
        == Some("1");

    if wants_icy && (session.track_title.is_some() || session.track_artist.is_some()) {
        headers.insert("icy-metaint", HeaderValue::from(ICY_METAINT as u64));
    }

    let sr = session.info.sample_rate;
    let bd = session.info.bit_depth;
    let ch = session.info.channels;
    let dur_ms = session.info.duration_ms;

    let has_icy = wants_icy && (session.track_title.is_some() || session.track_artist.is_some());
    let icy_block = if has_icy {
        build_icy_metadata(
            session.track_artist.as_deref(),
            session.track_title.as_deref(),
            session.cover_url.as_deref(),
        )
    } else {
        vec![0u8]
    };

    let body = Body::from_stream(async_stream::stream! {
        if is_wav {
            let hdr = build_wav_header_with_duration(ch, sr, bd, dur_ms);
            yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&hdr));
        }

        if has_icy {
            let mut bytes_since_meta: usize = 0;
            while let Some(chunk) = session.recv_chunk().await {
                let mut offset = 0;
                while offset < chunk.len() {
                    let remaining = ICY_METAINT - bytes_since_meta;
                    let end = (offset + remaining).min(chunk.len());
                    yield Ok(bytes::Bytes::copy_from_slice(&chunk[offset..end]));
                    bytes_since_meta += end - offset;
                    offset = end;
                    if bytes_since_meta >= ICY_METAINT {
                        yield Ok(bytes::Bytes::copy_from_slice(&icy_block));
                        bytes_since_meta = 0;
                    }
                }
            }
        } else {
            while let Some(chunk) = session.recv_chunk().await {
                yield Ok(bytes::Bytes::from(chunk));
            }
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

// ─── File serving with Range ────────────────────────────────────

async fn serve_file(path: &str, info: &StreamInfo, req_headers: &HeaderMap) -> Response {
    let file_path = std::path::Path::new(path);
    let file_size = match tokio::fs::metadata(file_path).await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let range_header = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let Some(range) = range_header {
        let range_str = range.replace("bytes=", "");
        let parts: Vec<&str> = range_str.split('-').collect();
        let start: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let end: u64 = parts
            .get(1)
            .and_then(|s| if s.is_empty() { None } else { s.parse().ok() })
            .unwrap_or(file_size - 1);
        let length = end - start + 1;

        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Type",
            HeaderValue::from_str(&info.mime_type).unwrap(),
        );
        headers.insert("Content-Length", HeaderValue::from(length));
        headers.insert(
            "Content-Range",
            HeaderValue::from_str(&format!("bytes {start}-{end}/{file_size}")).unwrap(),
        );
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );

        let path_owned = path.to_string();
        let body = Body::from_stream(async_stream::stream! {
            match tokio::fs::File::open(&path_owned).await {
                Ok(mut file) => {
                    use tokio::io::{AsyncReadExt, AsyncSeekExt};
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                        warn!(error = %e, "file_seek_error");
                        return;
                    }
                    let mut remaining = length;
                    let mut buf = vec![0u8; 65536];
                    while remaining > 0 {
                        let to_read = (remaining as usize).min(buf.len());
                        match file.read(&mut buf[..to_read]).await {
                            Ok(0) => break,
                            Ok(n) => {
                                remaining -= n as u64;
                                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                            }
                            Err(e) => {
                                warn!(error = %e, "file_read_error");
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "file_open_error"),
            }
        });

        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Full file
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&info.mime_type).unwrap(),
    );
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    let path_owned = path.to_string();
    let body = Body::from_stream(async_stream::stream! {
        match tokio::fs::File::open(&path_owned).await {
            Ok(mut file) => {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 65536];
                loop {
                    match file.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])),
                        Err(e) => {
                            warn!(error = %e, "file_read_error");
                            break;
                        }
                    }
                }
            }
            Err(e) => warn!(error = %e, "file_open_error"),
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

// ─── HTTPS→HTTP proxy ───────────────────────────────────────────

async fn proxy_stream(
    upstream_url: &str,
    info: &StreamInfo,
    is_radio: bool,
    req_headers: &HeaderMap,
) -> Response {
    let client = if is_radio {
        crate::http::client::long_timeout()
    } else {
        crate::http::client::long_timeout()
    };

    let upstream_resp = match client.get(upstream_url).send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, url = upstream_url, "proxy_upstream_error");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let upstream_content_type = upstream_resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&info.mime_type)
        .to_string();

    let content_length = upstream_resp
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&upstream_content_type).unwrap(),
    );
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    // DLNA renderers (e.g. Eversolo DMP-A8 with Lavf) send Range: bytes=0-
    // and expect 206 Partial Content with Content-Range header.
    // Returning 200 OK causes them to abort after ~31 seconds.
    let range_requested = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .filter(|r| r.starts_with("bytes=0-"));

    if let (Some(_), Some(cl)) = (range_requested, content_length) {
        headers.insert("Content-Length", HeaderValue::from(cl));
        headers.insert(
            "Content-Range",
            HeaderValue::from_str(&format!("bytes 0-{}/{}", cl - 1, cl)).unwrap(),
        );

        let body = Body::from_stream(async_stream::stream! {
            let mut stream = upstream_resp.bytes_stream();
            use futures_util::StreamExt;
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => yield Ok::<_, std::io::Error>(chunk),
                    Err(e) => {
                        warn!(error = %e, "proxy_chunk_error");
                        break;
                    }
                }
            }
        });

        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    if let Some(cl) = content_length {
        headers.insert("Content-Length", HeaderValue::from(cl));
    }

    let body = Body::from_stream(async_stream::stream! {
        let mut stream = upstream_resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => yield Ok::<_, std::io::Error>(chunk),
                Err(e) => {
                    warn!(error = %e, "proxy_chunk_error");
                    break;
                }
            }
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

pub fn router(sessions: SharedSessions) -> axum::Router {
    axum::Router::new()
        .route(
            "/stream/{stream_id}",
            axum::routing::get(handle_stream).head(handle_head),
        )
        .with_state(sessions)
}

fn build_icy_metadata(
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
