use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::streaming::deezer::DeezerService;
use crate::streaming::deezer_decrypt::DeezerDecryptStream;
use crate::streaming::ServiceRegistry;

pub async fn handle_deezer_proxy(
    Path(filename): Path<String>,
    State(services): State<Arc<Mutex<ServiceRegistry>>>,
) -> Response {
    let sng_id = filename.split('.').next().unwrap_or(&filename);
    if sng_id.is_empty() || !sng_id.chars().all(|c| c.is_ascii_digit()) {
        return (StatusCode::BAD_REQUEST, "invalid sng_id").into_response();
    }

    let ext = filename
        .rsplit('.')
        .next()
        .filter(|e| *e != sng_id)
        .unwrap_or("flac");
    let content_type = match ext {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    };

    let upstream_url = {
        let registry = services.lock().await;
        let svc = match registry.get("deezer") {
            Some(s) => s,
            None => return (StatusCode::NOT_FOUND, "deezer not registered").into_response(),
        };
        let svc = svc.lock().await;
        let deezer = match svc.as_any().downcast_ref::<DeezerService>() {
            Some(d) => d,
            None => return (StatusCode::INTERNAL_SERVER_ERROR, "not deezer").into_response(),
        };
        match deezer.get_full_stream_url(sng_id, 0).await {
            Ok(url) => url,
            Err(e) => {
                warn!(sng_id, error = %e, "deezer_proxy_no_upstream");
                return (StatusCode::NOT_FOUND, "upstream not available").into_response();
            }
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    // HEAD for Content-Length
    let content_length = client
        .head(&upstream_url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()
        .and_then(|r| {
            r.headers()
                .get("Content-Length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        });

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static(content_type));
    headers.insert("Accept-Ranges", HeaderValue::from_static("none"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );
    if let Some(cl) = content_length {
        headers.insert("Content-Length", HeaderValue::from(cl));
    }

    let upstream_resp = match client.get(&upstream_url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            warn!(sng_id, status = %r.status(), "deezer_proxy_upstream_status");
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
        Err(e) => {
            warn!(sng_id, error = %e, "deezer_proxy_upstream_error");
            return (StatusCode::BAD_GATEWAY, "upstream unreachable").into_response();
        }
    };

    info!(sng_id, content_type, "deezer_proxy_streaming");

    let sng_id_owned = sng_id.to_string();
    let body = Body::from_stream(async_stream::stream! {
        let mut decrypt = DeezerDecryptStream::new(&sng_id_owned);
        let mut stream = upstream_resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    let decrypted_chunks = decrypt.feed(&chunk);
                    for dc in decrypted_chunks {
                        yield Ok::<_, std::io::Error>(bytes::Bytes::from(dc));
                    }
                }
                Err(e) => {
                    warn!(error = %e, "deezer_proxy_chunk_error");
                    break;
                }
            }
        }
        if let Some(tail) = decrypt.finish() {
            yield Ok(bytes::Bytes::from(tail));
        }
    });

    (StatusCode::OK, headers, body).into_response()
}
