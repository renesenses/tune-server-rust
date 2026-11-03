//! #6466 (FabienM, fil 2302 points 11 et 15) — `POST /zones/{id}/queue/add`
//! pose le CONTENEUR de ce qu'il insère : « Lire ensuite » et « Ajouter à la
//! file » depuis un album, une playlist ou un artiste sont des déclencheurs au
//! même titre que « Lire ». Un titre ajouté seul est au premier niveau
//! (`track`) ; une liste nue sans contexte énoncé suit la session, comme
//! avant. Les témoins de l'historique lui-même sont dans tune-core
//! (`orchestrator/historique_conteneurs_tests_6466.rs`).
//!
//! Hermétique : base en mémoire, aucune zone ne joue.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ZONE: i64 = 1;

async fn banc() -> (tune_server::state::AppState, axum::Router) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO zones (id, name) VALUES (1, 'Salon');
             INSERT INTO artists (id, name) VALUES (1, 'Alain Bashung');
             INSERT INTO albums (id, title, artist_id) VALUES (7, 'Bleu pétrole', 1);
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, track_number)
               VALUES (71, 'Résidents de la République', 7, 1, '/m/71.flac', 200000, 1),
                      (72, 'Je t''ai manqué', 7, 1, '/m/72.flac', 200000, 2),
                      (80, 'Osez Joséphine', NULL, 1, '/m/80.flac', 200000, 1),
                      (81, 'Madame rêve', NULL, 1, '/m/81.flac', 200000, 2);",
        )
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

async fn ajouter(app: &axum::Router, corps: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post(format!("/api/v1/zones/{ZONE}/queue/add"))
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// `(track_id, context_type, context_id, context_source)` de chaque ligne.
fn file(
    state: &tune_server::state::AppState,
) -> Vec<(i64, Option<String>, Option<String>, Option<String>)> {
    state
        .backend
        .query_many(
            "SELECT track_id, context_type, context_id, context_source FROM queue_items \
             WHERE zone_id = 1 ORDER BY position, id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_i64().unwrap_or(0),
                r[1].as_string(),
                r[2].as_string(),
                r[3].as_string(),
            )
        })
        .collect()
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

/// 🔴 Un titre ajouté SEUL est au premier niveau : `track`.
#[tokio::test]
async fn un_titre_ajoute_seul_porte_track_6466() {
    let (state, app) = banc().await;
    assert_eq!(
        ajouter(&app, json!({ "track_id": 80 })).await,
        StatusCode::CREATED
    );
    assert_eq!(
        file(&state),
        vec![(80, s("track"), None, None)],
        "un titre ajouté seul doit être marqué `track` (premier niveau de l'historique)"
    );
}

/// 🔴 Un album entier ajouté porte l'album ; un contexte énoncé (artiste,
/// point 10 : « le conteneur de l'artiste qui contient les titres de
/// l'artiste joués ») porte ce contexte.
#[tokio::test]
async fn l_album_et_l_artiste_ajoutes_portent_leur_conteneur_6466() {
    let (state, app) = banc().await;
    assert_eq!(
        ajouter(&app, json!({ "album_id": 7 })).await,
        StatusCode::CREATED
    );
    assert_eq!(
        ajouter(
            &app,
            json!({ "track_ids": [80, 81], "context_type": "artist", "context_id": "1" })
        )
        .await,
        StatusCode::CREATED
    );
    assert_eq!(
        file(&state),
        vec![
            (71, s("album"), s("7"), s("local")),
            (72, s("album"), s("7"), s("local")),
            (80, s("artist"), s("1"), s("local")),
            (81, s("artist"), s("1"), s("local")),
        ],
        "chaque ajout doit porter SON conteneur, sans toucher aux lignes d'avant"
    );
}

/// Une liste nue sans contexte énoncé, ou un contexte inventé, ne devine
/// rien : NULL, la ligne suit la session comme avant.
#[tokio::test]
async fn une_liste_nue_ne_devine_pas_son_conteneur_6466() {
    let (state, app) = banc().await;
    assert_eq!(
        ajouter(&app, json!({ "track_ids": [80, 81] })).await,
        StatusCode::CREATED
    );
    assert_eq!(
        ajouter(
            &app,
            json!({ "track_ids": [71, 72], "context_type": "Album!" })
        )
        .await,
        StatusCode::CREATED
    );
    assert!(
        file(&state).iter().all(|l| l.1.is_none()),
        "aucun conteneur ne doit être deviné : {:?}",
        file(&state)
    );
}
