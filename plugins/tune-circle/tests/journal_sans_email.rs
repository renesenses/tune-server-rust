//! Tune Circle (#6935) : ni le pseudo saisi, ni l'URL (donc le jeton) d'un
//! lien d'invitation n'entrent au journal — à l'envoi, à la réponse, ni sur
//! une panne.
//!
//! Un binaire à lui seul, un seul test, un abonné GLOBAL posé avant tout le
//! reste, niveau TRACE, toutes cibles (même méthode que `journal_globe.rs`).

mod commun;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::json;

use commun::{JETON, adresse_morte, app, appel, base};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const PSEUDO: &str = "Pseudo-Temoin-6935";
const JETON_DU_LIEN: &str = "feedfacecafebeef6935feedfacecafebeef6935feedface";

/// Un cloud qui renvoie le corps reçu (pseudo) et un lien avec son URL.
async fn cloud_echo() -> String {
    let echo = |corps: Bytes| async move {
        (
            StatusCode::CREATED,
            [(header::CONTENT_TYPE, "application/json")],
            corps,
        )
            .into_response()
    };
    let app = Router::new()
        .route("/api/v1/circle/invitations/by-pseudo", post(echo))
        .route(
            "/api/v1/circle/invitation-links",
            post(|| async {
                (
                    StatusCode::CREATED,
                    axum::Json(json!({ "id": 1, "url": format!("https://mozaiklabs.fr/circle/rejoindre/{JETON_DU_LIEN}") })),
                )
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    format!("http://{adresse}")
}

#[tokio::test]
async fn ni_pseudo_ni_jeton_au_journal() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .without_time()
        .init();

    let app_ok = app(base(&cloud_echo().await, Some(JETON)));
    let r = appel(
        &app_ok,
        "POST",
        "/invitations/by-pseudo",
        Some(json!({ "pseudo": PSEUDO })),
    )
    .await;
    assert_eq!(r.json()["pseudo"], json!(PSEUDO), "le POST a bien traversé");
    let r = appel(&app_ok, "POST", "/invitation-links", None).await;
    assert!(r.json()["url"].as_str().unwrap().ends_with(JETON_DU_LIEN));
    let app_morte = app(base(&adresse_morte().await, Some(JETON)));
    appel(
        &app_morte,
        "POST",
        "/invitations/by-pseudo",
        Some(json!({ "pseudo": PSEUDO })),
    )
    .await;

    let journal = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // Contre-épreuve de la capture : un journal vide passerait tout.
    for attendu in [
        "circle_relai",
        "circle_cloud_injoignable",
        "POST /invitations/by-pseudo",
        "POST /invitation-links",
    ] {
        assert!(
            journal.contains(attendu),
            "évènement {attendu} absent de la capture"
        );
    }
    for secret in [PSEUDO, "Temoin-6935", JETON_DU_LIEN, "rejoindre"] {
        assert!(
            !journal.contains(secret),
            "le journal contient {secret:?} :\n{journal}"
        );
    }
}
