//! #7619 (L5) — la couche de compression gzip compresse le JSON, jamais
//! l'audio ni une réponse partielle.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;

fn corps() -> Vec<u8> {
    // Assez long et répétitif pour que gzip ait quelque chose à gagner.
    b"{\"albums\":[1,2,3]}".repeat(200)
}

fn routeur() -> Router {
    Router::new()
        .route(
            "/json",
            get(|| async { ([(header::CONTENT_TYPE, "application/json")], corps()) }),
        )
        .route(
            "/flac",
            get(|| async { ([(header::CONTENT_TYPE, "audio/flac")], corps()) }),
        )
        .route(
            "/brut",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    corps(),
                )
            }),
        )
        .route(
            "/partiel",
            get(|| async {
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, "text/plain"),
                        (header::CONTENT_RANGE, "bytes 0-3599/10000"),
                    ],
                    corps(),
                )
                    .into_response()
            }),
        )
        .layer(CompressionLayer::new().compress_when(super::predicat_de_compression()))
}

async fn encodage(uri: &str) -> Option<String> {
    let r = routeur()
        .oneshot(
            Request::get(uri)
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    r.headers()
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().unwrap().to_string())
}

#[tokio::test]
async fn le_json_est_compresse() {
    assert_eq!(encodage("/json").await.as_deref(), Some("gzip"));
}

/// Témoin : sans le prédicat, un flux `audio/flac` sortait gzippé.
#[tokio::test]
async fn l_audio_n_est_jamais_compresse() {
    assert_eq!(
        encodage("/flac").await,
        None,
        "un flux audio ne doit pas passer par gzip"
    );
    assert_eq!(encodage("/brut").await, None);
}

#[tokio::test]
async fn une_reponse_partielle_n_est_pas_compressee() {
    assert_eq!(
        encodage("/partiel").await,
        None,
        "une réponse 206 garde les octets que son Content-Range annonce"
    );
}
