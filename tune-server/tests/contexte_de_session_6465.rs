//! #6465 — un geste de lecture qui ne désigne aucun conteneur efface le
//! contexte de session laissé par le geste précédent.
//!
//! FabienM, fil 2302, points 7, 8 et 9 : après un album, une radio ou un
//! podcast se rangeait DANS cet album dans l'Historique, et la vignette « en
//! cours » renvoyait à lui. Seul `POST /zones/{id}/play` réécrivait le
//! contexte ; `POST /radios/{id}/play/{zone}`, `POST /podcasts/play/{zone}`
//! et `POST /zones/{id}/pins/{index}/invoke` laissaient en place celui de
//! l'album. La radio d'artiste est gardée dans `radio_artiste_5395.rs`, qui a
//! le service simulé qu'il lui faut.
//!
//! Hermétique : les adresses visent un port fermé de la boucle locale.

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
             INSERT INTO radio_stations (id, name, url) VALUES (9001, 'Station', 'http://127.0.0.1:9/flux');
             INSERT INTO settings (key, value) VALUES ('zone_1_pins',
               '[{\"index\":0,\"title\":\"Préréglage\",\"uri\":\"http://127.0.0.1:9/pin\",\"type\":\"radio\"}]');",
        )
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// Le contexte que laisse « Lire » sur l'album 101 (point 8 : « radio Agoria
/// suite à lecture album 101 »).
async fn apres_un_album(state: &tune_server::state::AppState) {
    state
        .playback
        .set_session_context(
            ZONE,
            Some("album".into()),
            Some("101".into()),
            Some("local".into()),
            Some("101".into()),
            None,
        )
        .await;
}

async fn poster(app: &axum::Router, uri: &str, corps: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn contexte(state: &tune_server::state::AppState) -> (Option<String>, Option<String>) {
    let etat = state.playback.get_state(ZONE).await;
    (etat.session_context_type, etat.session_context_id)
}

#[tokio::test]
async fn une_radio_jouee_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/radios/9001/play/1", json!({})).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "la station existe");
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "la radio a gardé le contexte de l'album 101 : l'Historique la range dans l'album"
    );
}

#[tokio::test]
async fn un_podcast_joue_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(
        &app,
        "/api/v1/podcasts/play/1",
        json!({"audio_url": "http://127.0.0.1:9/episode.mp3", "title": "Épisode", "podcast_id": "podcast-sans-album", "podcast_name": "Podcast"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        contexte(&state).await,
        (Some("podcast".into()), Some("podcast-sans-album".into())),
        "le podcast a gardé le contexte de l'album 101 : l'Historique le range dans l'album"
    );
    // Ancien client sans identifiant : aucun album ni podcast précédent ne fuit.
    assert_eq!(
        poster(
            &app,
            "/api/v1/podcasts/play/1",
            json!({
                "audio_url": "http://127.0.0.1:9/autre-episode.mp3", "title": "Autre épisode"
            })
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(contexte(&state).await, (Some("podcast".into()), None));
}

#[tokio::test]
async fn un_prereglage_invoque_apres_un_album_n_en_herite_pas_le_contexte() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/zones/1/pins/0/invoke", json!({})).await;
    assert_ne!(status, StatusCode::NOT_FOUND, "le préréglage existe");
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "le préréglage a gardé le contexte de l'album 101"
    );
}

/// Le refus ne touche à rien : une station inconnue laisse la session telle
/// qu'elle était.
#[tokio::test]
async fn une_station_inconnue_laisse_le_contexte_en_place() {
    let (state, app) = banc().await;
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/radios/999/play/1", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        contexte(&state).await,
        (Some("album".into()), Some("101".into()))
    );
}

/// L'album 202 de la bibliothèque, trois pistes.
fn un_album_de_trois_pistes(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO albums (id, title, source) VALUES (202, 'Album 202', 'local');
             INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) VALUES
               (2021, 'piste 1', 202, '/music/202-1.flac', 200000, 'flac'),
               (2022, 'piste 2', 202, '/music/202-2.flac', 200000, 'flac'),
               (2023, 'piste 3', 202, '/music/202-3.flac', 200000, 'flac');",
        )
        .unwrap();
}

/// Fil 2379, point 1 (FabienM, rc7) : « un album en lecture aléatoire → dans
/// l'historique on n'a pas le conteneur ». La lecture aléatoire d'un album
/// (`POST /playback/shuffle-all?album_id=`) pose l'album sur la session, comme
/// « Lire » : sans cela, la session gardait l'album 101 du geste précédent.
///
/// Le banc n'a pas de sortie : la lecture elle-même peut échouer, le contexte
/// est posé avant elle, comme dans `POST /zones/{id}/play`.
#[tokio::test]
async fn un_album_lu_en_aleatoire_est_le_conteneur_de_la_session() {
    let (state, app) = banc().await;
    un_album_de_trois_pistes(&state);
    apres_un_album(&state).await;
    let status = poster(
        &app,
        "/api/v1/playback/shuffle-all?zone_id=1&album_id=202",
        json!({}),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "l'album a des pistes");
    assert_eq!(
        contexte(&state).await,
        (Some("album".into()), Some("202".into())),
        "un album lu en aléatoire n'est pas le conteneur de l'Historique"
    );
}

/// La bibliothèque entière lue en aléatoire n'est pas un conteneur, et
/// n'hérite pas de l'album d'avant.
#[tokio::test]
async fn la_bibliotheque_lue_en_aleatoire_n_herite_pas_de_l_album() {
    let (state, app) = banc().await;
    un_album_de_trois_pistes(&state);
    apres_un_album(&state).await;
    let status = poster(&app, "/api/v1/playback/shuffle-all?zone_id=1", json!({})).await;
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "la bibliothèque a des pistes"
    );
    assert_eq!(
        contexte(&state).await,
        (None, None),
        "un tirage de la bibliothèque s'est rangé dans l'album 101"
    );
}
