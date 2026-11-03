//! #6155 (Pierre, fil 2220) — regrouper un Devialet Phantom et une KEF LS50 II
//! en DLNA doit faire jouer LES DEUX.
//!
//! Le serveur répondait `201` au `POST /zones/group` du client web, rangeait
//! le groupe dans `zone_groups`… et plus rien : aucun module de lecture ne
//! lisait ce réglage. La lecture lancée sur le Phantom (meneuse) restait sur
//! le Phantom ; la KEF (suiveuse) ne recevait jamais rien.
//!
//! Le banc : deux renderers DLNA factices, le groupe formé par la route
//! qu'appelle le client web, le relais de groupe lancé comme au démarrage.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par son entrée `[[test]]` dans `Cargo.toml`.
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::PlayState;
use tune_server::routes::zones::relais_de_groupe_6155::lancer_le_relais_de_groupe;
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

/// Les titres que le renderer factice a reçus.
async fn joues_par(state: &AppState, device_id: &str) -> Vec<String> {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(device_id).expect("sortie enregistree");
    let garde = sortie.lock().await;
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    mock.play_titles().await
}

/// Attend (au plus 10 s) que `condition` soit vraie : le relais agit dans une
/// tâche de fond.
async fn attendre<F, Fut>(mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

struct Banc {
    app: axum::Router,
    state: AppState,
    phantom: i64,
    kef: i64,
    _dir: tempfile::TempDir,
}

/// Deux zones DLNA, chacune sur son renderer factice, une piste FLAC réelle en
/// file sur le Phantom, le relais lancé comme le fait `spawn_background_tasks`.
async fn phantom_et_kef() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // Le regroupement relève du droit « Multiroom Sync ».
    state.license.set_account_premium(true, None).await;
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
    let phantom = zones
        .create("Phantom 95 dB", Some("dlna"), Some("dlna-phantom"))
        .unwrap();
    let kef = zones
        .create("KEF LS50 II", Some("dlna"), Some("dlna-kef"))
        .unwrap();
    {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(
            MockOutput::new("dlna-phantom", "Phantom 95 dB").with_type("dlna"),
        ));
        registre.register(Box::new(
            MockOutput::new("dlna-kef", "KEF LS50 II").with_type("dlna"),
        ));
    }
    lancer_le_relais_de_groupe(&state);

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{phantom}/queue/add"),
        json!({ "track_ids": [1] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");

    Banc {
        app,
        state,
        phantom,
        kef,
        _dir: dir,
    }
}

/// Le corps exact du client web (`groupZones(zone.id, [zone.id, …])`).
async fn grouper(banc: &Banc) {
    let (status, corps) = poster(
        &banc.app,
        "/api/v1/zones/group",
        json!({ "leader_id": banc.phantom, "zone_ids": [banc.phantom, banc.kef] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "regroupement : {corps}");
}

#[tokio::test]
async fn la_lecture_lancee_sur_le_groupe_sort_sur_les_deux_enceintes() {
    let banc = phantom_et_kef().await;
    grouper(&banc).await;

    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur la meneuse");
    assert_eq!(
        joues_par(&banc.state, "dlna-phantom").await,
        vec!["Canopée".to_string()],
        "témoin : la meneuse joue"
    );

    let kef_joue = attendre(|| async {
        joues_par(&banc.state, "dlna-kef").await == vec!["Canopée".to_string()]
    })
    .await;
    assert!(
        kef_joue,
        "la KEF, groupée avec le Phantom, doit recevoir la piste qu'il joue — \
         reçu : {:?}. Vide : le groupe est enregistré mais personne ne le lit, \
         le défaut de #6155.",
        joues_par(&banc.state, "dlna-kef").await
    );
    assert_eq!(
        joues_par(&banc.state, "dlna-phantom").await.len(),
        1,
        "rejoindre le groupe ne relance pas la meneuse"
    );

    // Pause du groupe depuis la meneuse : la suiveuse se tait aussi.
    let (status, corps) = poster(
        &banc.app,
        &format!("/api/v1/zones/{}/pause", banc.phantom),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "pause : {corps}");
    let kef_en_pause = attendre(|| async {
        banc.state.playback.get_state(banc.kef).await.state == PlayState::Paused
    })
    .await;
    assert!(
        kef_en_pause,
        "la pause de la meneuse doit mettre la KEF en pause"
    );

    // Arrêt du groupe : la suiveuse s'arrête aussi (après le délai de grâce).
    let (status, corps) = poster(
        &banc.app,
        &format!("/api/v1/zones/{}/stop", banc.phantom),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "arrêt : {corps}");
    let kef_arretee = attendre(|| async {
        banc.state.playback.get_state(banc.kef).await.state == PlayState::Stopped
    })
    .await;
    assert!(kef_arretee, "l'arrêt de la meneuse doit arrêter la KEF");
}

#[tokio::test]
async fn grouper_une_zone_qui_joue_deja_fait_jouer_l_autre() {
    let banc = phantom_et_kef().await;
    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur le Phantom seul");

    grouper(&banc).await;

    let kef_joue = attendre(|| async {
        joues_par(&banc.state, "dlna-kef").await == vec!["Canopée".to_string()]
    })
    .await;
    assert!(
        kef_joue,
        "former le groupe autour du Phantom en lecture doit faire jouer la KEF \
         — reçu : {:?}",
        joues_par(&banc.state, "dlna-kef").await
    );
}

#[tokio::test]
async fn une_zone_hors_groupe_reste_muette() {
    let banc = phantom_et_kef().await;
    banc.state
        .orchestrator
        .play_from_queue(banc.phantom, 0)
        .await
        .expect("lecture sur le Phantom seul");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        joues_par(&banc.state, "dlna-kef").await.is_empty(),
        "sans groupe, le relais ne touche à aucune autre zone"
    );
}
