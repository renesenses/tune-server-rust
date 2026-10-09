//! Ticket 134 — trois « suivant » en rafale, par les ROUTES MONTÉES.
//!
//! Le terrain : trois « suivant » rapprochés ont avancé de trois pistes. C'est la règle voulue : un appui = une piste, comme le
//! bouton l'annonce. Mais elle ne tenait que par chance : `next` lance
//! `play_from_queue` en tâche de fond, et c'est cette tâche qui avance la
//! position de la zone, après ses lectures de base. Un « suivant » arrivé
//! avant elle relisait l'ancienne position et visait la même piste — sur un
//! hôte dont la base est lente, trois appuis n'avançaient que d'une piste.
//!
//! Ici, les trois appels sont servis sur un exécuteur à un seul fil, sans que
//! les tâches de lecture aient pu tourner entre eux : le pire cas, rendu
//! déterministe.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(chemin)
        .header("X-Profile-Id", "1")
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
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

/// Un album de `n` pistes, une zone avec une sortie factice, la file posée
/// sur la première piste.
async fn banc(n: usize) -> (axum::Router, AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    let ar = ArtistRepo::with_backend(state.backend.clone())
        .get_or_create("Artiste", None, None)
        .expect("artiste");
    let al = AlbumRepo::with_backend(state.backend.clone())
        .get_or_create("Album", ar.id.unwrap(), None)
        .expect("album");
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for i in 0..n {
        let mut t = Track::new(format!("Piste {i:02}"));
        t.artist_id = ar.id;
        t.album_id = al.id;
        t.format = Some("flac".into());
        t.duration_ms = 200_000;
        t.track_number = i as i32 + 1;
        t.file_path = Some(format!("/musique/album/{i:02}.flac"));
        ids.push(repo.create(&t).expect("piste"));
    }
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new("mock-salon", "Sortie d'essai").with_type("mock"),
    ));
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("salon", Some("mock"), Some("mock-salon"))
        .expect("zone");
    let (st, v) = poster(
        &app,
        &format!("/api/v1/zones/{zone}/queue/add"),
        json!({ "track_ids": ids }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "mise en file : {v}");
    state.playback.update_queue_info(zone, 0, n as i64).await;
    (app, state, zone)
}

/// Trois appuis rapprochés = trois pistes, chacune visée UNE fois.
#[tokio::test(flavor = "current_thread")]
async fn trois_suivants_en_rafale_visent_trois_pistes_distinctes() {
    let (app, _state, zone) = banc(8).await;
    let chemin = format!("/api/v1/zones/{zone}/next");
    let (a, b, c) = tokio::join!(
        poster(&app, &chemin, json!({})),
        poster(&app, &chemin, json!({})),
        poster(&app, &chemin, json!({})),
    );
    let mut positions: Vec<i64> = [a, b, c]
        .iter()
        .map(|(st, v)| {
            assert_eq!(*st, StatusCode::OK, "{v}");
            v["queue_position"].as_i64().expect("queue_position")
        })
        .collect();
    positions.sort();
    assert_eq!(
        positions,
        vec![1, 2, 3],
        "chaque appui doit avancer d'une piste, même quand la lecture du \
         précédent n'a pas encore posé sa position"
    );
}

/// Témoin : appuis espacés (la lecture du précédent a abouti) — la règle est
/// la même, et la position de la zone fait foi.
#[tokio::test(flavor = "current_thread")]
async fn des_suivants_espaces_avancent_d_une_piste_chacun() {
    let (app, state, zone) = banc(8).await;
    let chemin = format!("/api/v1/zones/{zone}/next");
    for attendu in 1..=3 {
        let (st, v) = poster(&app, &chemin, json!({})).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["queue_position"], json!(attendu), "{v}");
        // Laisser la tâche de lecture poser la position de la zone.
        for _ in 0..200 {
            if state.playback.get_state(zone).await.queue_position == attendu {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(state.playback.get_state(zone).await.queue_position, attendu);
    }
}
