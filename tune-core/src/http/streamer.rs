use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::audio::wav::build_wav_header;

const ICY_METAINT: usize = 16384;
const CHUNK_SIZE: usize = 65536;

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
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub track_album: Option<String>,
    pub cover_url: Option<String>,
    pub bit_perfect: bool,
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
            track_title: None,
            track_artist: None,
            track_album: None,
            cover_url: None,
            bit_perfect,
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
        self.sessions.lock().await.insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "stream_session_created");
        (id, tx)
    }

    pub async fn set_metadata(
        &self,
        stream_id: &str,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        cover_url: Option<String>,
    ) {
        // Metadata is set at creation; for live updates we'd need interior mutability
        // This is a placeholder for the Python integration
        debug!(stream_id, "set_metadata_called");
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

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_str(&session.info.mime_type).unwrap());
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert("transferMode.dlna.org", HeaderValue::from_static("Streaming"));

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

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_str(&session.info.mime_type).unwrap());
    headers.insert("transferMode.dlna.org", HeaderValue::from_static("Streaming"));
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
        // WAV header injection
        if is_wav {
            let hdr = build_wav_header(ch, sr, bd);
            yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&hdr));
        }

        // Stream audio chunks
        loop {
            match session.recv_chunk().await {
                Some(chunk) => {
                    yield Ok(bytes::Bytes::from(chunk));
                }
                None => break,
            }
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

pub fn router(sessions: SharedSessions) -> axum::Router {
    axum::Router::new()
        .route("/stream/{stream_id}", axum::routing::get(handle_stream).head(handle_head))
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

        let url = streamer.get_stream_url(&id, "192.168.1.18", "wav");
        assert!(url.contains(&id));
        assert!(url.ends_with(".wav"));

        streamer.remove_session(&id).await;
    }

    #[test]
    fn stream_id_extraction() {
        assert_eq!(extract_stream_id("abc123.flac"), "abc123");
        assert_eq!(extract_stream_id("abc123"), "abc123");
    }
}
