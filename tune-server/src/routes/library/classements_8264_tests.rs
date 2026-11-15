//! #8264 — `GET /library/stats` sert `rankings` (artistes et genres classés
//! par nombre de pistes) sans rien changer au reste, de bout en bout par le
//! routeur (SQLite).
//!
//! Le témoin de la demande : « combien de titres de Jean-Sébastien Bach ?
//! combien de titres du genre concertos pour violon ? » — avant ce correctif,
//! la réponse de la route ne portait ni l'un ni l'autre (`rankings` absent).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;

async fn stats(state: &AppState) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/library/stats")
        .body(Body::empty())
        .unwrap();
    let reponse = app.oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn les_stats_classent_artistes_et_genres_par_pistes_8264() {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = s.backend.clone();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Johann Sebastian Bach'), (2, 'Antonio Vivaldi'), (3, '  ')",
        "INSERT INTO albums (id, title, genre, genres) VALUES \
            (10, 'Concertos BWV 1041-1043', 'Violin Concerto', NULL), \
            (11, 'Les Quatre Saisons', NULL, '[\"Baroque\",\"Violin-Concerto\"]'), \
            (12, 'Variations Goldberg', 'Baroque', NULL), \
            (13, 'Sans genre', NULL, NULL)",
        "INSERT INTO tracks (id, album_id, artist_id, title, file_path) VALUES \
            (1, 10, 1, 'a', '/m/1'), (2, 10, 1, 'b', '/m/2'), (3, 10, 1, 'c', '/m/3'), \
            (4, 11, 2, 'd', '/m/4'), (5, 11, 2, 'e', '/m/5'), \
            (6, 12, 1, 'f', '/m/6'), \
            (7, 13, 3, 'g', '/m/7'), \
            (8, NULL, NULL, 'h', '/m/8')",
    ] {
        b.execute(sql, &[]).unwrap();
    }
    let (st, v) = stats(&s).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v["rankings"],
        json!({
            "by_artist": {
                "items": [
                    {"value": "Johann Sebastian Bach", "tracks": 4},
                    {"value": "Antonio Vivaldi", "tracks": 2},
                ],
                "total": 2,
            },
            "by_genre": {
                // 3 pistes « Violin Concerto » + 2 « Violin-Concerto » : un seul genre.
                "items": [
                    {"value": "Violin Concerto", "tracks": 5},
                    {"value": "Baroque", "tracks": 3},
                ],
                "total": 2,
            },
        }),
    );
    // Rien d'existant ne bouge.
    assert_eq!(v["tracks"], 8);
    assert!(v["distributions"].is_object());
}

async fn appel(state: &AppState, uri: &str) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let reponse = app.oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Go du 13/11 — la recherche porte sur TOUT le classement, au-delà des cent
/// premiers de la carte, et rend le rang dans le classement complet.
#[tokio::test]
async fn la_recherche_trouve_au_dela_des_cent_premiers() {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = s.backend.clone();
    b.execute(
        "INSERT INTO albums (id, title, genre) VALUES (1, 'A', 'Baroque'), (2, 'B', 'Opéra')",
        &[],
    )
    .unwrap();
    // 120 artistes de 2 pistes, et Dvořák avec une seule : 121ᵉ.
    let mut piste = 1;
    for i in 1..=121 {
        let nom = if i == 121 {
            "Antonín Dvořák".to_string()
        } else {
            format!("Artiste {i:03}")
        };
        b.execute(
            &format!("INSERT INTO artists (id, name) VALUES ({i}, '{nom}')"),
            &[],
        )
        .unwrap();
        let n = if i == 121 { 1 } else { 2 };
        for _ in 0..n {
            let album = if i == 121 { 2 } else { 1 };
            b.execute(
                &format!(
                    "INSERT INTO tracks (id, album_id, artist_id, title, file_path) \
                     VALUES ({piste}, {album}, {i}, 't', '/m/{piste}')"
                ),
                &[],
            )
            .unwrap();
            piste += 1;
        }
    }
    let (_, v) = stats(&s).await;
    assert_eq!(v["rankings"]["by_artist"]["total"], 121);
    assert_eq!(
        v["rankings"]["by_artist"]["items"]
            .as_array()
            .unwrap()
            .len(),
        100
    );

    let (st, v) = appel(&s, "/api/v1/library/stats/rankings?by=artist&q=dvorak").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v,
        json!({
            "by": "artist",
            "items": [{"rank": 121, "value": "Antonín Dvořák", "tracks": 1}],
            "matched": 1,
            "total": 121,
        })
    );

    let (st, v) = appel(&s, "/api/v1/library/stats/rankings?by=genre&q=OPERA").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v["items"],
        json!([{"rank": 2, "value": "Opéra", "tracks": 1}])
    );
    assert_eq!(v["total"], 2);

    let (_, v) = appel(
        &s,
        "/api/v1/library/stats/rankings?by=artist&q=artiste&limit=3",
    )
    .await;
    assert_eq!(v["matched"], 120);
    assert_eq!(v["items"].as_array().unwrap().len(), 3);

    let (st, _) = appel(&s, "/api/v1/library/stats/rankings?by=label").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}
