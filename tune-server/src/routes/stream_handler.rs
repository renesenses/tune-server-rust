//! Axum HTTP handlers for audio streaming.
//!
//! The business logic (session management, buffer handling) lives in
//! `tune_core::http::streamer`. This module provides the HTTP layer only.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

use tune_core::http::streamer::{
    ICY_METAINT, SharedSessions, StreamInfo, StreamSession, build_icy_metadata, build_wav_header,
    extract_stream_id,
};

pub async fn handle_head(
    Path(raw_id): Path<String>,
    State(sessions): State<SharedSessions>,
) -> Response {
    let stream_id = extract_stream_id(&raw_id);
    // Clone the Arc so we release the sessions lock before any async I/O.
    // Holding the global sessions lock across tokio::fs::metadata() (an async
    // syscall) would serialize ALL concurrent stream requests — HEAD and GET
    // included — on a single lock, causing unnecessary latency on renderers
    // that issue HEAD+GET in quick succession (DMP-A8, darTZeel, etc.).
    let session = {
        let sessions = sessions.lock().await;
        sessions.get(stream_id).cloned()
    };

    let Some(session) = session else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // For file sessions, read actual size from filesystem (consistent with GET)
    let file_size = if session.info.file_size.is_some() {
        session.info.file_size
    } else {
        let fp = session.file_path.lock().await;
        if let Some(ref path) = *fp {
            tokio::fs::metadata(path.as_str())
                .await
                .ok()
                .map(|m| m.len())
        } else {
            session.info.wav_content_length()
        }
    };

    let is_radio = session.is_radio;

    info!(
        stream_id,
        format = %session.info.format,
        file_size = ?file_size,
        is_radio,
        "stream_head_request"
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&session.info.mime_type).unwrap(),
    );
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    if is_radio {
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );
        headers.insert("Transfer-Encoding", HeaderValue::from_static("chunked"));
    } else {
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Interactive"),
        );
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );
        if let Some(size) = file_size {
            headers.insert("Content-Length", HeaderValue::from(size));
        }
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
    session.first_request.notify_waiters();

    // File serving with Range support
    let file_path = session.file_path.lock().await.clone();
    if let Some(ref path) = file_path {
        return serve_file(path, &session.info, &req_headers, session.clone()).await;
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
    let is_radio = session.is_radio;
    let wav_length = if is_wav {
        session.info.wav_content_length()
    } else {
        None
    };

    // DLNA renderers (Marantz SR7009, Eversolo DMP-A8) send Range: bytes=0-
    // even for the initial request and expect a 206 Partial Content response
    // with Content-Range.  Without this, they reject the stream and stop
    // playback.  When we know the content length, honour the Range request
    // by responding with 206 + Content-Range.
    let range_bytes_zero = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .filter(|r| r.starts_with("bytes=0-"));
    let use_partial = range_bytes_zero.is_some() && wav_length.is_some();

    if let Some(len) = wav_length {
        headers.insert("Content-Length", HeaderValue::from(len));
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        if use_partial {
            headers.insert(
                "Content-Range",
                HeaderValue::from_str(&format!("bytes 0-{}/{}", len - 1, len)).unwrap(),
            );
        }
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

    let wav_header_included = session
        .wav_header_included
        .load(std::sync::atomic::Ordering::Relaxed);
    let body = Body::from_stream(async_stream::stream! {
        if is_wav && !wav_header_included {
            let hdr = build_wav_header(ch, sr, bd, dur_ms);
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
        } else if is_radio {
            // Radio streams are infinite — yield chunks immediately for
            // real-time playback.  The coalescing buffer used for finite
            // tracks adds latency that is acceptable for Squeezebox/LMS
            // but can cause the browser's <audio> element (or the local
            // output's HTTP reader) to stall waiting for the first data
            // after the WAV header, resulting in silence.
            while let Some(chunk) = session.recv_chunk().await {
                yield Ok(bytes::Bytes::from(chunk));
            }
        } else {
            // Coalesce small chunks into larger HTTP writes (target >=64 KB).
            // Network outputs like Squeezebox/LMS fetch audio from this HTTP
            // stream.  Yielding many small chunks (~32 KB each from the decoder)
            // causes per-write overhead and can trigger micro-pauses that manifest
            // as audible stuttering/crackling on the player.  Buffering to >=64 KB
            // gives the network renderer more data per TCP segment, reducing the
            // chance of buffer underrun.
            const MIN_HTTP_CHUNK: usize = 65536;
            let mut coalesce_buf = Vec::with_capacity(MIN_HTTP_CHUNK * 2);
            while let Some(chunk) = session.recv_chunk().await {
                coalesce_buf.extend_from_slice(&chunk);
                while coalesce_buf.len() >= MIN_HTTP_CHUNK {
                    let flushed: Vec<u8> = coalesce_buf.drain(..MIN_HTTP_CHUNK).collect();
                    yield Ok(bytes::Bytes::from(flushed));
                }
            }
            // Flush any remaining bytes at end of stream
            if !coalesce_buf.is_empty() {
                yield Ok(bytes::Bytes::from(coalesce_buf));
            }
        }
    });

    let status = if use_partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    (status, headers, body).into_response()
}

// ─── File serving with Range ────────────────────────────────────

async fn serve_file(
    path: &str,
    info: &StreamInfo,
    req_headers: &HeaderMap,
    session: std::sync::Arc<StreamSession>,
) -> Response {
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
            HeaderValue::from_static("Interactive"),
        );
        headers.insert("Connection", HeaderValue::from_static("keep-alive"));
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );

        let path_owned = path.to_string();
        // Track served bytes so the poller can tell an actively-fetching
        // renderer from a genuinely-stalled one (fixes false force-stop of
        // DLNA renderers that report Stopped while streaming — Linn, RS130).
        let byte_counter = session.clone();
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
                                byte_counter.bytes_sent.fetch_add(
                                    n as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
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
        HeaderValue::from_static("Interactive"),
    );
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert(
        "contentFeatures.dlna.org",
        HeaderValue::from_static("DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
    );

    let path_owned = path.to_string();
    let byte_counter = session.clone();
    let body = Body::from_stream(async_stream::stream! {
        match tokio::fs::File::open(&path_owned).await {
            Ok(mut file) => {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 65536];
                loop {
                    match file.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            byte_counter.bytes_sent.fetch_add(
                                n as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
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
        // Radio streams are infinite — use a client with no total timeout
        // so the connection stays alive until the user stops playback.
        tune_core::http::client::infinite_stream()
    } else {
        tune_core::http::client::long_timeout()
    };

    // Parse the Range header once so we can decide how to fetch upstream.
    let range_value = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // DLNA renderers (Eversolo DMP-A8 with Lavf) seek by issuing mid-file Range
    // requests (bytes=N-). Forward the requested start offset to the CDN and pass
    // its 206 straight through, so the renderer gets exactly the bytes it asked
    // for. Previously the Range was dropped and a non-zero range fell through to
    // a 200 full-from-0 body: the renderer never found its offset, re-requested
    // in a loop, and each loop re-fetched the whole FLAC from Qobuz until the CDN
    // dropped the body ("error decoding response body"). Radio is infinite and
    // not seekable, so we never forward a range for it.
    let range_start: Option<u64> = if is_radio {
        None
    } else {
        range_value
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.split('-').next())
            .and_then(|s| s.parse::<u64>().ok())
    };

    let mut upstream_req = client.get(upstream_url);
    if let Some(start) = range_start {
        upstream_req = upstream_req.header("Range", format!("bytes={start}-"));
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, url = upstream_url, "proxy_upstream_error");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let upstream_status = upstream_resp.status();
    let upstream_content_range = upstream_resp
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

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
    if !is_radio {
        // Only advertise Accept-Ranges for finite streams.  Radio streams
        // are infinite — advertising seekability causes some browsers to
        // attempt byte-range requests that will never succeed.
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    }
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    // Radio streams are infinite — no Content-Length is possible.
    // The DMP-A8 sends Range: bytes=0- initially, then reconnects with
    // bytes=N- (resume). Both must return 206 with an open-ended
    // Content-Range so the renderer keeps consuming the stream.
    let any_range = range_value.as_deref().filter(|r| r.starts_with("bytes="));
    if is_radio && any_range.is_some() {
        headers.remove("Accept-Ranges");
        headers.insert("Content-Range", HeaderValue::from_static("bytes 0-*/*"));
        headers.insert("Transfer-Encoding", HeaderValue::from_static("chunked"));

        info!(url = upstream_url, "proxy_radio_206_open_ended");

        let body = Body::from_stream(async_stream::stream! {
            let mut stream = upstream_resp.bytes_stream();
            use futures_util::StreamExt;
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => yield Ok::<_, std::io::Error>(chunk),
                    Err(e) => {
                        warn!(error = %e, "proxy_radio_chunk_error");
                        break;
                    }
                }
            }
        });

        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // The CDN honoured our forwarded Range: pass its 206 (with the CDN's own
    // Content-Range) straight through so the renderer gets the requested offset.
    if range_start.is_some() && upstream_status == StatusCode::PARTIAL_CONTENT {
        if let Some(ref cr) = upstream_content_range {
            if let Ok(v) = HeaderValue::from_str(cr) {
                headers.insert("Content-Range", v);
            }
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
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Fallback: a Range was requested but the CDN returned a full 200 (didn't
    // honour it). DLNA renderers reject a 200 for a Range request and stop after
    // ~31s, so synthesize a 206 spanning the whole file.
    let range_requested = range_value.as_deref().filter(|r| r.starts_with("bytes=0-"));
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
