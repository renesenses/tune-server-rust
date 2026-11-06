//! #6465 (FabienM, fil 2379 point 5, second volet) — « Lire un album, puis
//! ajouter un autre album […] quand on clique sur la vignette du titre en
//! cours, ça ouvre l'album du titre, mais ce sont les titres de l'album des
//! Catchers qui sont listés ».
//!
//! `GET /zones/{id}/album-en-cours` lisait le contexte de SESSION (le dernier
//! « Lire ») avant tout : sur une piste du second album, ajouté à la file, il
//! renvoyait le premier. Le conteneur de la ligne jouée (`queue_items.context_*`,
//! #6466) passe désormais devant.
//!
//! Hermétique : base en mémoire, aucune sortie, aucun son.

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
             INSERT INTO artists (id, name) VALUES (1, 'The Catchers'), (2, 'Chilly Gonzales');
             INSERT INTO albums (id, title, artist_id) VALUES (7, 'We Speak in Frames', 1),
                                                           (9, 'iTunes Live: Berlin Festival', 2);
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, track_number)
               VALUES (71, 'Un', 7, 1, '/m/71.flac', 200000, 1),
                      (72, 'Deux', 7, 1, '/m/72.flac', 200000, 2),
                      (91, 'Trois', 9, 2, '/m/91.flac', 200000, 1),
                      (92, 'Quatre', 9, 2, '/m/92.flac', 200000, 2);",
        )
        .unwrap();
    // Le « Lire » du premier album : le contexte de session.
    state
        .playback
        .set_session_context(
            ZONE,
            Some("album".into()),
            Some("7".into()),
            Some("local".into()),
            None,
            None,
        )
        .await;
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

async fn ajouter(app: &axum::Router, corps: Value) {
    let st = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/zones/{ZONE}/queue/add"))
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::CREATED, "ajout refusé : {corps}");
}

/// La zone joue la piste `track_id` (album `album_id`) au rang `position`.
async fn jouer(state: &tune_server::state::AppState, track_id: i64, album_id: i64, position: i64) {
    state
        .playback
        .play(
            ZONE,
            tune_core::playback::NowPlaying {
                title: format!("piste {track_id}"),
                source: "local".into(),
                track_id: Some(track_id),
                album_id: Some(album_id),
                ..Default::default()
            },
        )
        .await;
    state.playback.update_queue_info(ZONE, position, 4).await;
}

async fn album_en_cours(app: &axum::Router) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/zones/{ZONE}/album-en-cours"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 🔴 Le cas du fil : sur une piste du SECOND album, ajouté à la file, la
/// vignette ouvre le second album, pas celui de la session.
#[tokio::test]
async fn le_second_album_ajoute_ouvre_sa_propre_fiche_6465() {
    let (state, app) = banc().await;
    ajouter(&app, json!({ "album_id": 7 })).await;
    ajouter(&app, json!({ "album_id": 9 })).await;
    jouer(&state, 91, 9, 2).await;

    let (status, body) = album_en_cours(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["album_id"], "9",
        "sur une piste du second album, la vignette a ouvert l'album de la session \
         (le premier) : {body}"
    );
    assert_eq!(body["path"], "/api/v1/library/albums/9");
    assert_eq!(body["origin"], "queue_context");
}

/// Un titre ajouté SEUL (`track`) n'appartient pas à l'album de la session :
/// c'est l'album de la piste elle-même qui répond.
#[tokio::test]
async fn un_titre_ajoute_seul_ne_prend_pas_l_album_de_la_session_6465() {
    let (state, app) = banc().await;
    ajouter(&app, json!({ "album_id": 7 })).await;
    ajouter(&app, json!({ "track_id": 92 })).await;
    jouer(&state, 92, 9, 2).await;

    let (status, body) = album_en_cours(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["album_id"], "9", "{body}");
    assert_eq!(body["origin"], "current_track");
}

/// Non-régression #1361 : une ligne du premier album, et une ligne sans
/// conteneur propre (liste nue), gardent la réponse d'avant.
#[tokio::test]
async fn sans_conteneur_propre_le_geste_de_session_repond_toujours_6465() {
    let (state, app) = banc().await;
    ajouter(&app, json!({ "track_ids": [71, 72] })).await;
    jouer(&state, 72, 7, 1).await;

    let (status, body) = album_en_cours(&app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["album_id"], "7", "{body}");
    assert_eq!(body["origin"], "session_context");
}
