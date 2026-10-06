//! #5770 — une insertion AVANT la piste en cours fait suivre le curseur.
//!
//! « Lire à partir d'ici » sur une liste de service lance le titre cliqué,
//! enfile la suite, puis remet les titres PRÉCÉDENTS en tête de file
//! (`POST /zones/{id}/queue/add` avec `position: 0`). `queue_add` gardait
//! l'ancien curseur : il désignait alors une ligne insérée et non la piste
//! en cours, si bien que « Précédent » la rejouait.
//!
//! Ce que ce fichier cloue, sur la route montée :
//!
//! 1. insérer à la position du curseur ou avant lui le décale du nombre de
//!    lignes ÉCRITES ;
//! 2. insérer après lui, ou dans une file vide, ne le bouge pas ;
//! 3. la réponse porte `queue_position`, le curseur après l'insertion : c'est
//!    à sa présence que le client reconnaît un serveur qui sait le faire.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn requete(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn enfiler(app: &axum::Router, zone_id: i64, corps: Value) -> Value {
    let (status, rep) = requete(
        app,
        Request::post(format!("/api/v1/zones/{zone_id}/queue/add"))
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {rep}");
    rep
}

async fn file(app: &axum::Router, zone_id: i64) -> Value {
    let (status, rep) = requete(
        app,
        Request::get(format!("/api/v1/zones/{zone_id}/queue"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rep}");
    rep
}

fn piste(state: &AppState, titre: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.format = Some("flac".into());
    t.file_path = Some(format!("/musique/{titre}.flac"));
    t.duration_ms = 240_000;
    TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste")
}

fn de_service(id: &str) -> Value {
    json!({ "source": "qobuz", "source_id": id, "title": id, "artist_name": "X" })
}

/// La file `[A, B, C]`, curseur sur `B` (rang 1) : c'est `B` qui joue.
async fn file_sur_b(app: &axum::Router, state: &AppState) -> i64 {
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:defaut"))
        .unwrap();
    let ids: Vec<i64> = ["A", "B", "C"].iter().map(|t| piste(state, t)).collect();
    enfiler(app, zone_id, json!({ "track_ids": ids })).await;
    state.playback.update_queue_info(zone_id, 1, 3).await;
    zone_id
}

#[tokio::test]
async fn inserer_en_tete_fait_suivre_la_piste_en_cours() {
    let (app, state) = app_et_etat();
    let zone_id = file_sur_b(&app, &state).await;

    let rep = enfiler(
        &app,
        zone_id,
        json!({ "tracks": [de_service("p1"), de_service("p2")], "position": 0 }),
    )
    .await;
    assert_eq!(rep["position"], 0, "{rep}");
    assert_eq!(
        state.playback.get_state(zone_id).await.queue_position,
        3,
        "deux lignes insérées avant B : le curseur doit passer de 1 à 3, sinon \
         il désigne une ligne insérée et « Précédent » rejoue la piste en cours"
    );
    assert_eq!(
        rep["queue_position"], 3,
        "la réponse dit le curseur : {rep}"
    );

    let f = file(&app, zone_id).await;
    assert_eq!(f["position"], 3, "{f}");
    assert_eq!(f["tracks"][3]["title"], "B", "le curseur pointe B : {f}");
    assert_eq!(
        f["tracks"][2]["source_id"], "p2",
        "et B a sa précédente : {f}"
    );
}

#[tokio::test]
async fn inserer_a_la_place_du_curseur_le_decale_aussi() {
    let (app, state) = app_et_etat();
    let zone_id = file_sur_b(&app, &state).await;
    enfiler(
        &app,
        zone_id,
        json!({ "tracks": [de_service("x")], "position": 1 }),
    )
    .await;
    assert_eq!(state.playback.get_state(zone_id).await.queue_position, 2);
}

#[tokio::test]
async fn inserer_apres_le_curseur_ne_le_bouge_pas() {
    let (app, state) = app_et_etat();
    let zone_id = file_sur_b(&app, &state).await;

    // « Lire ensuite » : juste après la piste en cours.
    let rep = enfiler(
        &app,
        zone_id,
        json!({ "tracks": [de_service("x")], "position": 2 }),
    )
    .await;
    assert_eq!(state.playback.get_state(zone_id).await.queue_position, 1);
    assert_eq!(rep["queue_position"], 1, "{rep}");

    // En fin de file.
    enfiler(&app, zone_id, json!({ "tracks": [de_service("y")] })).await;
    assert_eq!(state.playback.get_state(zone_id).await.queue_position, 1);
}

#[tokio::test]
async fn une_file_vide_n_a_pas_de_curseur_a_decaler() {
    let (app, state) = app_et_etat();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:defaut"))
        .unwrap();
    let rep = enfiler(
        &app,
        zone_id,
        json!({ "tracks": [de_service("a"), de_service("b")], "position": 0 }),
    )
    .await;
    assert_eq!(state.playback.get_state(zone_id).await.queue_position, 0);
    assert_eq!(rep["queue_position"], 0, "{rep}");
}
