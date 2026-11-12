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
