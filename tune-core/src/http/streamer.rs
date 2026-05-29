use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::audio::wav::build_wav_header;

const ICY_METAINT: usize = 16384;

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub format: String,
    pub mime_type: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub file_size: Option<u64>,
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
        return proxy_stream(url, &session.info, session.is_radio).await;
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

    let wants_icy = req_headers
        .get("Icy-MetaData")
        .and_then(|v| v.to_str().ok())
        == Some("1");

    if wants_icy && (session.track_title.is_some() || session.track_artist.is_some()) {
        headers.insert("icy-metaint", HeaderValue::from(ICY_METAINT as u64));
    }

    let is_wav = session.info.format == "wav";
    let sr = session.info.sample_rate;
    let bd = session.info.bit_depth;
    let ch = session.info.channels;

    let body = Body::from_stream(async_stream::stream! {
        if is_wav {
            let hdr = build_wav_header(ch, sr, bd);
            yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&hdr));
        }
        while let Some(chunk) = session.recv_chunk().await {
            yield Ok(bytes::Bytes::from(chunk));
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

async fn proxy_stream(upstream_url: &str, info: &StreamInfo, is_radio: bool) -> Response {
    let timeout = if is_radio {
        std::time::Duration::from_secs(0) // no total timeout for radio
    } else {
        std::time::Duration::from_secs(600)
    };

    let client = reqwest::Client::builder()
        .timeout(if is_radio {
            std::time::Duration::from_secs(86400)
        } else {
            timeout
        })
        .build();

    let Ok(client) = client else {
        return StatusCode::BAD_GATEWAY.into_response();
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
        };
        let id = streamer
            .create_file_session(info, "/music/test.flac".into(), true)
            .await;
        let url = streamer.get_stream_url(&id, "192.168.1.18", "flac");
        assert!(url.contains(".flac"));
        streamer.remove_session(&id).await;
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
}
