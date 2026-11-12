//! #8479 (Ronin22, AudioScienceReview #169, rc11) — la fiche artiste ne montre
//! plus aucun album Spotify, seulement ceux de Qobuz.
//!
//! Cause : `get_artist_albums` demandait `limit=50`. Depuis février 2026,
//! `GET /artists/{id}/albums` n'accepte qu'un `limit` de 0 à 10 et répond
//! `400 Invalid limit` au-delà. Le faux Spotify ci-dessous applique cette
//! règle et pagine par `offset`, comme Spotify.

use super::*;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;
use std::collections::HashMap;

const TOTAL: usize = 23;

async fn faux_spotify_2026() -> SpotifyService {
    async fn albums(
        Path(_id): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> (StatusCode, axum::Json<serde_json::Value>) {
        let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(5);
        let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
        if limit > 10 {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"status": 400, "message": "Invalid limit"}})),
            );
        }
        let fin = (offset + limit).min(TOTAL);
        let items: Vec<serde_json::Value> = (offset..fin)
            .map(|i| {
                json!({"id": format!("al{i}"), "name": format!("Album {i}"),
                       "artists": [{"id": "ar", "name": "Moonchild"}],
                       "images": [], "release_date": "2019-01-01", "total_tracks": 10})
            })
            .collect();
        let next = if fin < TOTAL {
            json!(format!(
                "https://api.spotify.com/v1/artists/ar/albums?offset={fin}"
            ))
        } else {
            serde_json::Value::Null
        };
        (
            StatusCode::OK,
            axum::Json(json!({"items": items, "total": TOTAL, "limit": limit,
                              "offset": offset, "next": next})),
        )
    }
    let app = axum::Router::new().route("/v1/artists/{id}/albums", get(albums));
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
    svc.user_id = Some("ronin".into());
    svc
}

#[tokio::test]
async fn spotify_8479_la_discographie_se_lit_par_pages_de_dix() {
    let svc = faux_spotify_2026().await;
    let albums = svc.get_artist_albums("ar").await.expect(
        "#8479 : `limit` au-dessus de 10 → `400 Invalid limit`, la fiche perd ses albums Spotify",
    );
    let ids: Vec<String> = albums.iter().map(|a| a.id.clone()).collect();
    let attendus: Vec<String> = (0..TOTAL).map(|i| format!("al{i}")).collect();
    assert_eq!(
        ids, attendus,
        "#8479 : toutes les pages, dans l'ordre, sans doublon"
    );
}

#[tokio::test]
async fn spotify_8479_la_route_paginee_rend_tout_a_l_offset_zero() {
    let svc = faux_spotify_2026().await;
    let page = svc.get_artist_albums_page("ar", 0).await.expect("page 0");
    assert_eq!(page.len(), TOTAL);
    let suite = svc
        .get_artist_albums_page("ar", 50)
        .await
        .expect("page suivante");
    assert!(
        suite.is_empty(),
        "le « voir plus » du client s'arrête proprement"
    );
}
