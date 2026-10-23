//! Tune Circle rc8 (#6900) : ni le libellé ni les coordonnées d'un lieu
//! n'entrent au journal — à l'envoi, à la lecture, ni sur un refus.
//!
//! Un binaire à lui seul, un seul test, un abonné GLOBAL posé avant tout le
//! reste, niveau TRACE, toutes cibles (même méthode que `journal.rs`).

mod commun;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
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

const LIBELLE: &str = "Ville-Temoin-6900";

/// Un cloud qui renvoie le corps reçu (PUT) et un lieu de contact (GET).
async fn cloud_echo() -> String {
    let echo = |corps: Bytes| async move {
        ([(header::CONTENT_TYPE, "application/json")], corps).into_response()
    };
    let lieu = || async {
        axum::Json(json!({ "label": LIBELLE, "country_code": "NO", "lat": 69.5, "lon": 18.5 }))
    };
    let app = Router::new()
        .route("/api/v1/circle/me/location", get(lieu).put(echo))
        .route(
            "/api/v1/circle/contacts/locations",
            get(|| async {
                axum::Json(json!([{ "contact_id": 7, "label": LIBELLE, "lat": 69.5, "lon": 18.5 }]))
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
async fn ni_libelle_ni_coordonnees_au_journal() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        // Sans horodatage : « 20:31:18.512 » contiendrait « 18.5 ».
        .without_time()
        .init();

    let corps = json!({ "label": LIBELLE, "country_code": "no", "lat": 69.6492, "lon": 18.9553 });
    let app_ok = app(base(&cloud_echo().await, Some(JETON)));
    let r = appel(&app_ok, "PUT", "/me/location", Some(corps.clone())).await;
    assert_eq!(r.json()["lat"], json!(69.5), "le PUT a bien traversé");
    appel(&app_ok, "GET", "/me/location", None).await;
    appel(&app_ok, "GET", "/contacts/locations", None).await;
    let mut hors_bornes = corps.clone();
    hors_bornes["lat"] = json!(96.6492);
    appel(&app_ok, "PUT", "/me/location", Some(hors_bornes)).await;
    let app_morte = app(base(&adresse_morte().await, Some(JETON)));
    appel(&app_morte, "PUT", "/me/location", Some(corps)).await;

    let journal = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // Contre-épreuve de la capture : un journal vide passerait tout.
    for attendu in [
        "circle_relai",
        "circle_cloud_injoignable",
        "PUT /me/location",
    ] {
        assert!(
            journal.contains(attendu),
            "évènement {attendu} absent de la capture"
        );
    }
    for secret in [
        LIBELLE,
        "Temoin-6900",
        "69.6",
        "69.5",
        "18.9",
        "18.5",
        "96.6",
    ] {
        assert!(
            !journal.contains(secret),
            "le journal contient {secret:?} :\n{journal}"
        );
    }
}
