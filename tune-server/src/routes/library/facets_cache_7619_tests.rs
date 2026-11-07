//! #7619 (L4) — le cache des compteurs de facettes ne sert jamais un état
//! périmé : chaque écriture dans une table suivie change la révision de la
//! bibliothèque, et le rail suivant est recalculé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::state::AppState;

async fn rail(app: &axum::Router, uri: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let corps = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&corps).unwrap()
}

fn effectif(rail: &Value, champ: &str, valeur: &str) -> Option<i64> {
    rail[champ].as_array()?.iter().find_map(|e| {
        (e["value"].as_str() == Some(valeur)).then(|| e["count"].as_i64().unwrap_or(0))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_rail_suit_chaque_ecriture_et_le_cache_sert_le_reste() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("facettes-cache.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'A');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1), (2, 'C', 1);
             INSERT INTO tracks (id, title, album_id, artist_id, genre, file_path) VALUES
               (1, 'x', 1, 1, 'Jazz', '/m/1.flac'), (2, 'y', 1, 1, 'Jazz', '/m/2.flac'),
               (3, 'z', 2, 1, 'Rock', '/m/3.flac');",
        )
        .unwrap();
    assert!(
        tune_core::db::library_revision::lire(state.backend.as_ref()).is_some(),
        "le serveur pose la révision au démarrage"
    );
    let app = crate::routes::router(state.clone());
    let uri = "/api/v1/library/facets?fields=genre";

    let r1 = rail(&app, uri).await;
    assert_eq!(effectif(&r1, "genre", "Jazz"), Some(2), "{r1}");
    // Servi par le cache : identique.
    assert_eq!(rail(&app, uri).await, r1);

    // Une piste de plus : le rail suit.
    state
        .backend
        .execute_batch(
            "INSERT INTO tracks (id, title, album_id, artist_id, genre, file_path) \
             VALUES (4, 'w', 2, 1, 'Jazz', '/m/4.flac');",
        )
        .unwrap();
    let r2 = rail(&app, uri).await;
    assert_eq!(
        effectif(&r2, "genre", "Jazz"),
        Some(3),
        "piste ajoutée : {r2}"
    );

    // Un album masqué : ses pistes sortent du rail.
    state
        .backend
        .execute_batch("INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 2);")
        .unwrap();
    let r3 = rail(&app, uri).await;
    assert_eq!(
        effectif(&r3, "genre", "Jazz"),
        Some(2),
        "album masqué : {r3}"
    );
    assert_eq!(effectif(&r3, "genre", "Rock"), None, "album masqué : {r3}");

    // Un genre modifié.
    state
        .backend
        .execute_batch("UPDATE tracks SET genre = 'Blues' WHERE id = 1;")
        .unwrap();
    let r4 = rail(&app, uri).await;
    assert_eq!(
        effectif(&r4, "genre", "Blues"),
        Some(1),
        "genre modifié : {r4}"
    );
}
