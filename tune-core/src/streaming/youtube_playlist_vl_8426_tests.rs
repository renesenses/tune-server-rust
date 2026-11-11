//! #8426 — l'Historique redemande chaque playlist YouTube jouée avec son
//! identifiant BRUT (`PL…`). InnerTube rend 400 sur `browseId=PL…` et 200 sur
//! `browseId=VLPL…` (mesuré le 11/11/2026 sur `music.youtube.com`).
//!
//! Aucun réseau : un serveur local tient le rôle d'InnerTube et se comporte
//! comme lui sur ce point (400 sans `VL`).
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::*;

const BRUT: &str = "PLFgquLnL59alCl_2TQvOiD5Vgm1hCaGSI";

async fn faux_browse(
    State(appels): State<Arc<StdMutex<Vec<String>>>>,
    Json(corps): Json<Value>,
) -> Response {
    let id = corps["browseId"].as_str().unwrap_or("").to_string();
    appels.lock().unwrap().push(id.clone());
    if !id.starts_with("VL") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": {"code": 400, "message": "Request contains an invalid argument.", "status": "INVALID_ARGUMENT"}})),
        )
            .into_response();
    }
    Json(json!({
        "header": {"musicDetailHeaderRenderer": {
            "title": {"runs": [{"text": "Ma playlist"}]}
        }}
    }))
    .into_response()
}

async fn service() -> (YouTubeService, Arc<StdMutex<Vec<String>>>) {
    let appels = Arc::new(StdMutex::new(Vec::new()));
    let app = Router::new()
        .route("/browse", post(faux_browse))
        .with_state(appels.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await });
    let base = format!("http://{adresse}");
    (YouTubeService::avec_bases(&base, &base), appels)
}

/// Sans compte ni clé (le cas du signalement) : la playlist est trouvée, sous
/// l'identifiant demandé, et InnerTube a reçu `VLPL…`.
#[tokio::test]
async fn get_playlist_avec_un_identifiant_brut_pl_trouve_la_playlist_8426() {
    let (svc, appels) = service().await;
    let p = svc
        .get_playlist(BRUT)
        .await
        .expect("la playlist doit être trouvée (400 sans VL, puis 404)");
    assert_eq!(p.name, "Ma playlist");
    assert_eq!(p.id, BRUT, "l'identifiant rendu reste celui demandé");
    assert_eq!(
        appels.lock().unwrap().first().map(String::as_str),
        Some(format!("VL{BRUT}").as_str())
    );
}

#[tokio::test]
async fn get_playlist_tracks_envoie_vl_a_innertube_8426() {
    let (svc, appels) = service().await;
    let _ = svc.get_playlist_tracks(BRUT).await;
    assert_eq!(
        appels.lock().unwrap().first().map(String::as_str),
        Some(format!("VL{BRUT}").as_str())
    );
}

/// Un identifiant déjà préfixé (ceux de la découverte) n'est pas doublé.
#[tokio::test]
async fn get_playlist_ne_double_pas_vl_8426() {
    let (svc, appels) = service().await;
    let deja = format!("VL{BRUT}");
    svc.get_playlist(&deja).await.expect("playlist");
    assert_eq!(appels.lock().unwrap().as_slice(), [deja]);
}
