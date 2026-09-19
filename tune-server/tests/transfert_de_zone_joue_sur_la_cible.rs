//! Transférer la lecture vers une autre zone doit la faire JOUER sur la cible.
//!
//! Mesuré sur la .18 le 17/09/2026 (Bertrand) : Eversolo → Décodeur TV, le
//! journal portait `orchestrator_play_retap_deduped_same_inflight_track
//! zone_id=17` et rien d'autre. La cible restait muette, la source était
//! arrêtée : la musique disparaissait.
//!
//! `do_transfer` posait l'état de la cible (`playback.play`, donc « en
//! lecture », même morceau, horodaté à l'instant) AVANT d'appeler
//! l'orchestrateur, dont l'anti-double-appui voyait alors un second tap.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans `server_contracts.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_server::state::AppState;

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
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
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Deux zones réseau, chacune sur sa sortie factice, et une piste FLAC réelle
/// en file sur la première, en lecture.
/// Ce que la sortie factice a reçu : titres joués, nombre d'arrêts.
async fn recu_par(state: &AppState, device_id: &str) -> (Vec<String>, u64) {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(device_id).expect("sortie enregistree");
    let garde = sortie.lock().await;
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    (mock.play_titles().await, mock.stop_call_count())
}

async fn deux_zones_dont_une_joue() -> (axum::Router, AppState, i64, i64, tempfile::TempDir) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("piste.flac");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../tune-core/tests/fixtures/test.flac"
        ),
        &chemin,
    )
    .unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                                 duration_ms, sample_rate, bit_depth, channels, source) \
             VALUES (1, 'Canopée', 1, 1, '{}', 'flac', 300000, 44100, 16, 2, 'local');",
            chemin.to_string_lossy()
        ))
        .unwrap();

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let source = zones
        .create("Eversolo", Some("dlna"), Some("dlna-source"))
        .unwrap();
    let cible = zones
        .create("Décodeur TV", Some("dlna"), Some("dlna-cible"))
        .unwrap();
    {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(
            MockOutput::new("dlna-source", "Eversolo").with_type("dlna"),
        ));
        registre.register(Box::new(
            MockOutput::new("dlna-cible", "Décodeur TV").with_type("dlna"),
        ));
    }

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{source}/queue/add"),
        json!({ "track_ids": [1] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");
    state
        .orchestrator
        .play_from_queue(source, 0)
        .await
        .expect("lecture sur la source");
    assert_eq!(
        recu_par(&state, "dlna-source").await.0.len(),
        1,
        "témoin : la source joue avant le transfert"
    );

    (app, state, source, cible, dir)
}

#[tokio::test]
async fn le_transfert_envoie_la_lecture_a_l_appareil_cible() {
    let (app, state, source, cible, _dir) = deux_zones_dont_une_joue().await;

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{source}/transfer/{cible}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "transfert : {corps}");

    let (joues_cible, _) = recu_par(&state, "dlna-cible").await;
    assert_eq!(
        joues_cible.len(),
        1,
        "l'appareil cible doit recevoir la lecture. À 0, l'anti-double-appui \
         a pris le transfert pour un second tap — le défaut de la .18."
    );
    assert_eq!(joues_cible, vec!["Canopée".to_string()]);
    assert!(
        recu_par(&state, "dlna-source").await.1 >= 1,
        "la source est arrêtée une fois la cible lancée"
    );
    let etat = state.playback.get_state(cible).await;
    assert_eq!(
        etat.now_playing.map(|np| np.title),
        Some("Canopée".to_string())
    );
}
