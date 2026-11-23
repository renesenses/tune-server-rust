use super::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);
impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Journal {
    fn lignes(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}

#[tokio::test]
async fn not_found_garde_le_chemin_des_requetes_concurrentes_8901() {
    let journal = Journal::default();
    let writer = journal.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route(
                "/albums/{id}/versions",
                axum::routing::get(|| async {
                    tokio::task::yield_now().await;
                    AppError::not_found("album absent")
                }),
            )
            .route(
                "/artists/{id}",
                axum::routing::get(|| async {
                    tokio::task::yield_now().await;
                    AppError::not_found("artiste absent")
                }),
            )
            .layer(axum::middleware::from_fn(avec_chemin_requete));
        let request = |uri| {
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let (a, b) = tokio::join!(
            router
                .clone()
                .oneshot(request("/albums/123/versions?query=non-journalisee")),
            router.oneshot(request("/artists/456"))
        );
        for response in [a.unwrap(), b.unwrap()] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], "not_found");
        }
    }
    .with_subscriber(subscriber)
    .await;
    let events = journal.lignes();
    assert_eq!(events.len(), 2);
    for (message, path) in [
        ("album absent", "/albums/{id}/versions"),
        ("artiste absent", "/artists/{id}"),
    ] {
        let event = events
            .iter()
            .find(|e| e["fields"]["error"] == message)
            .unwrap();
        assert_eq!(
            event["fields"]["path"], path,
            "not_found doit nommer sa route"
        );
    }
    assert!(
        !String::from_utf8(journal.0.lock().unwrap().clone())
            .unwrap()
            .contains("non-journalisee")
    );
}

#[tokio::test]
async fn not_found_hors_requete_reste_valide_8901() {
    let journal = Journal::default();
    let writer = journal.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let error = AppError::not_found("absent hors HTTP");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.message, "absent hors HTTP");
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(journal.lignes()[0]["fields"]["message"], "not_found");
}

#[tokio::test]
async fn internal_error_nomme_sa_route_9343() {
    let journal = Journal::default();
    let writer = journal.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    async {
        let router = axum::Router::new()
            .route(
                "/converter/start",
                axum::routing::post(|| async { AppError::internal("disque refuse") }),
            )
            .layer(axum::middleware::from_fn(avec_chemin_requete));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/converter/start")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
    .with_subscriber(subscriber)
    .await;
    let events = journal.lignes();
    let event = events
        .iter()
        .find(|e| e["fields"]["error"] == "disque refuse")
        .unwrap();
    assert_eq!(event["fields"]["path"], "/converter/start");
}

#[test]
fn lecture_seule_message_dedie_qui_nomme_le_chemin_9343() {
    let chemin = std::path::Path::new("/plugins/audio/jobs");
    let erofs = std::io::Error::from_raw_os_error(30);
    let message = message_d_entree_sortie("write job journal", chemin, &erofs);
    if erofs.kind() == std::io::ErrorKind::ReadOnlyFilesystem {
        assert!(message.contains("read-only file system"), "{message}");
    }
    assert!(message.contains("/plugins/audio/jobs"), "{message}");
    assert!(message.starts_with("write job journal"), "{message}");
    let autre = std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem);
    let message = AppError::io("write job journal", chemin, &autre).message;
    assert!(
        message.contains("/plugins/audio/jobs is on a read-only file system"),
        "{message}"
    );
}
