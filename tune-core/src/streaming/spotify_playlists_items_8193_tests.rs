//! #8193 (fil 2715, Jon Rotaetxe, 09/11) — après la connexion à Spotify, les
//! playlists s'ouvrent VIDES, y compris celles que l'utilisateur a créées.
//!
//! Cause : la migration de l'API Web du 9 mars 2026 (applications en mode
//! développement) a retiré `GET /playlists/{id}/tracks`, `POST
//! /playlists/{id}/tracks` et `POST /users/{id}/playlists`, et renommé les
//! champs : `tracks` → `items`, `tracks.items[].track` → `items.items[].item`.
//!
//! Le faux Spotify ci-dessous parle l'API d'aujourd'hui : les anciennes routes
//! y répondent 403, comme chez Spotify.

use super::*;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Recu(Arc<Mutex<Vec<String>>>);

fn titre(i: usize) -> serde_json::Value {
    json!({"id": format!("t{i}"), "name": format!("titre {i}"), "duration_ms": 1000,
           "artists": [{"id": "ar", "name": "Artiste"}], "album": {"id": "al", "name": "Album"}})
}

async fn faux_spotify_2026() -> (SpotifyService, Recu) {
    async fn refus() -> (StatusCode, axum::Json<serde_json::Value>) {
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": {"status": 403, "message": "Forbidden"}})),
        )
    }
    async fn playlist(Path(id): Path<String>) -> axum::Json<serde_json::Value> {
        axum::Json(json!({"id": id, "name": "Ma playlist", "images": [],
                          "owner": {"display_name": "Jon"},
                          "items": {"total": 3, "items": []}}))
    }
    async fn elements(Path(_id): Path<String>) -> axum::Json<serde_json::Value> {
        axum::Json(json!({"items": [
            {"added_at": "2026-11-09T10:00:00Z", "item": titre(0)},
            {"added_at": "2026-11-09T10:00:00Z", "item": titre(1)},
            {"added_at": "2026-11-09T10:00:00Z", "item": null},
            {"added_at": "2026-11-09T10:00:00Z", "item": titre(2)}
        ], "total": 4, "limit": 100, "offset": 0, "next": null}))
    }
    async fn mes_playlists() -> axum::Json<serde_json::Value> {
        axum::Json(
            json!({"items": [{"id": "p1", "name": "Ma playlist", "images": [],
                                     "owner": {}, "items": {"total": 3}}],
                          "total": 1, "next": null}),
        )
    }
    async fn creer(State(r): State<Recu>) -> axum::Json<serde_json::Value> {
        r.0.lock().unwrap().push("creer".into());
        axum::Json(json!({"id": "nouvelle"}))
    }
    async fn ajouter(
        State(r): State<Recu>,
        Path(id): Path<String>,
    ) -> axum::Json<serde_json::Value> {
        r.0.lock().unwrap().push(format!("ajouter:{id}"));
        axum::Json(json!({"snapshot_id": "s"}))
    }
    let recu = Recu::default();
    let app = axum::Router::new()
        .route("/v1/playlists/{id}", get(playlist))
        .route("/v1/playlists/{id}/items", get(elements).post(ajouter))
        .route("/v1/playlists/{id}/tracks", get(refus).post(refus))
        .route("/v1/me/playlists", get(mes_playlists).post(creer))
        .route("/v1/users/{id}/playlists", post(refus))
        .with_state(recu.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse locale"));
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    svc.user_id = Some("jon".into());
    (svc, recu)
}

#[tokio::test]
async fn les_titres_d_une_playlist_remontent_avec_l_api_2026_8193() {
    let (svc, _) = faux_spotify_2026().await;
    let titres = svc
        .get_playlist_tracks("p1")
        .await
        .expect("#8193 : `GET /playlists/{id}/tracks` est retiré (403) — la route est `/items`");
    let ids: Vec<&str> = titres.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        ["t0", "t1", "t2"],
        "#8193 : chaque entrée porte `item` (et non plus `track`) ; une entrée sans titre est sautée"
    );
}

#[tokio::test]
async fn le_nombre_de_titres_se_lit_sous_items_8193() {
    let (svc, _) = faux_spotify_2026().await;
    let p = svc.get_playlist("p1").await.expect("playlist");
    assert_eq!(
        p.track_count, 3,
        "#8193 : `tracks.total` est devenu `items.total`"
    );
    let mes = svc.get_user_playlists().await.expect("mes playlists");
    assert_eq!(
        mes[0].track_count, 3,
        "#8193 : `tracks.total` est devenu `items.total`"
    );
}

#[tokio::test]
async fn creer_et_remplir_une_playlist_passent_par_les_nouvelles_routes_8193() {
    let (svc, recu) = faux_spotify_2026().await;
    let id = svc
        .create_playlist("Essai", None)
        .await
        .expect("#8193 : `POST /users/{id}/playlists` est retiré — la route est `/me/playlists`");
    assert_eq!(id, "nouvelle");
    let n = svc
        .add_tracks_to_playlist("nouvelle", &["t0".into(), "t1".into()])
        .await
        .expect("#8193 : `POST /playlists/{id}/tracks` est retiré — la route est `/items`");
    assert_eq!(n, 2);
    assert_eq!(
        *recu.0.lock().unwrap(),
        ["creer".to_string(), "ajouter:nouvelle".to_string()]
    );
}
