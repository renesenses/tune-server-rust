//! #7090 — contrat des similaires artistes depuis un album, sans cloud réel.
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tune_core::db::{artist_repo::ArtistRepo, models::Artist, settings_repo::SettingsRepo};
const MBID_CONNU: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
const MBID: &str = "8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11";

#[tokio::test]
async fn les_similaires_d_album_lisent_le_contrat_cloud_de_reference_7090() {
    let appels = Arc::new(Mutex::new(Vec::<String>::new()));
    let compteur = appels.clone();
    let cloud = Router::new().route("/api/v1/artists/{mbid}/similar", get(move |Path(mbid): Path<String>| {
        let compteur = compteur.clone();
        async move {
            compteur.lock().unwrap().push(mbid.clone());
            assert!([MBID, MBID_CONNU].contains(&mbid.as_str()));
            Json(json!({"data":{"musicbrainz_id":mbid,"similar_artists":[{"name":"Massive Attack"}]}}))
        }
    })).route("/api/v1/artists/search", get(|Query(q): Query<std::collections::HashMap<String,String>>| async move {
        assert_eq!(q.get("q").map(String::as_str), Some("Portishead"));
        Json(json!({"data":[{"name":"Portishead","musicbrainz_id":MBID}],"count":1}))
    }));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1/artists", ecoute.local_addr().unwrap());
    let serveur = tokio::spawn(async move {
        axum::serve(ecoute, cloud).await.unwrap();
    });
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", &base)
        .unwrap();
    let artistes = ArtistRepo::with_backend(state.backend.clone());
    let mut connu = Artist::new("Radiohead".into());
    connu.musicbrainz_id = Some(MBID_CONNU.into());
    let id_connu = artistes.create(&connu).unwrap();
    let id_sans_mbid = artistes.create(&Artist::new("Portishead".into())).unwrap();
    for (album_id, artiste_id) in [(1i64, id_connu), (2, id_sans_mbid)] {
        state
            .backend
            .execute(
                "INSERT INTO albums (id, title, artist_id) VALUES (?1, 'Dummy', ?2)",
                &[&album_id, &artiste_id],
            )
            .unwrap();
    }
    let app = crate::routes::router(state);
    for id in [1, 2] {
        let reponse = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/library/albums/{id}/similar"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reponse.status(), StatusCode::OK);
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let data: Value = serde_json::from_slice(&octets).unwrap();
        assert_eq!(
            data["artists"][0]["name"], "Massive Attack",
            "#7090 : la route doit lire data.similar_artists au bon chemin cloud, album={id}, réponse={data}"
        );
        assert_eq!(data["album"], "Dummy");
        assert_eq!(data["mbid"], if id == 1 { MBID_CONNU } else { MBID });
    }
    assert_eq!(appels.lock().unwrap().len(), 2);
    let absent = app
        .oneshot(
            Request::get("/api/v1/library/albums/999/similar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    serveur.abort();
}
