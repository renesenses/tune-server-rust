//! #7894 (point 9 de #7778) — Pause sur un appareil qui ne joue rien : la
//! route ne rend plus 502, elle rend la zone ARRÊTÉE et une phrase traduite
//! qui dit que rien ne jouait sur l'appareil.
//!
//! L'appareil est un `MockOutput` de type `dlna` armé « sans média »
//! (`with_sans_media`) : sa pause n'a rien à suspendre et il le signale par
//! `take_nothing_playing_on_pause()`, comme le `DlnaOutput` devant un
//! renderer en `NO_MEDIA_PRESENT` (témoins de tune-core,
//! `orchestrator/pause_sans_media_7894_tests.rs`). Aucun son.
//!
//! Contre-épreuve : dans `PlaybackOrchestrator::pause_avec_issue`, ne plus
//! lire `take_nothing_playing_on_pause()` (forcer `false`) fait tomber
//! `une_pause_sans_media_rend_une_zone_arretee_et_le_dit` : la zone est
//! annoncée « paused ».

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::{NowPlaying, PlayState};

use crate::state::AppState;

const APPAREIL: &str = "uuid:salon-sans-media-7894";

async fn zone_sur_appareil_sans_media() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon DLNA", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon")
            .with_type("dlna")
            .with_sans_media(),
    ));
    state
        .playback
        .play(
            zid,
            NowPlaying {
                title: "Épisode".into(),
                source: "podcast".into(),
                source_id: Some("https://h.example/media/42".into()),
                duration_ms: 1_800_000,
                ..Default::default()
            },
        )
        .await;
    state.playback.update_position(zid, 42_000).await;
    (state, zid)
}

async fn pause(state: &AppState, zid: i64, langue: &str) -> (StatusCode, Value) {
    let reponse = crate::routes::router(state.clone())
        .oneshot(
            Request::post(format!("/api/v1/zones/{zid}/pause"))
                .header("accept-language", langue)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), 1024 * 1024).await.unwrap();
    (statut, serde_json::from_slice(&octets).unwrap())
}

#[tokio::test]
async fn une_pause_sans_media_rend_une_zone_arretee_et_le_dit() {
    let (state, zid) = zone_sur_appareil_sans_media().await;
    let (statut, corps) = pause(&state, zid, "fr").await;

    assert_eq!(statut, StatusCode::OK, "plus de 502 : {corps}");
    assert_eq!(
        corps["state"], "stopped",
        "rien ne jouait : la zone est arrêtée, ni « playing » ni « paused » : {corps}"
    );
    assert_eq!(corps["code"], "nothing_playing_on_device", "{corps}");
    assert_eq!(
        corps["message_key"], "api.playback.nothingPlayingOnDevice",
        "{corps}"
    );
    let message = corps["message"].as_str().unwrap();
    assert!(message.contains("Salon DLNA"), "{message}");
    assert!(message.contains("Rien ne joue"), "{message}");

    // Reprise possible : la piste reste en mémoire.
    let etat = state.playback.get_state(zid).await;
    assert_eq!(etat.state, PlayState::Stopped);
    assert!(
        etat.now_playing.is_some(),
        "la piste est gardée pour la reprise"
    );
}

#[tokio::test]
async fn la_phrase_suit_la_langue_de_l_interface() {
    let (state, zid) = zone_sur_appareil_sans_media().await;
    let (_, corps) = pause(&state, zid, "en").await;
    let message = corps["message"].as_str().unwrap();
    assert!(
        message.starts_with("Nothing is playing on Salon DLNA"),
        "{message}"
    );
}

/// Ce que la sortie factice a reçu : les types annoncés à `play_media`.
async fn types_recus(state: &AppState) -> Vec<String> {
    let registre = state.outputs.lock().await;
    let sortie = registre.get(APPAREIL).expect("sortie enregistrée");
    let garde = sortie.lock().await;
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    mock.play_mime_types().await
}

async fn poster(state: &AppState, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let reponse = crate::routes::router(state.clone())
        .oneshot(
            Request::post(chemin)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), 1024 * 1024).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// **La reprise possible, de bout en bout.** Un M4A servi sous une adresse
/// SANS extension (l'hébergeur dit `audio/mp4`) est lancé sur l'appareil, qui
/// n'a rien chargé : la pause arrête la zone. Lecture rejoue alors l'épisode
/// par l'orchestrateur — et l'appareil doit recevoir le MÊME type qu'au
/// lancement, pas l'`audio/mpeg` que l'adresse seule ferait annoncer.
///
/// Contre-épreuve : dans la route `resume`, repasser `media_format: None` fait
/// tomber ce témoin sur « audio/mpeg ».
#[tokio::test]
async fn lecture_apres_une_pause_sans_media_rejoue_l_episode_avec_son_type() {
    use axum::{Router, routing::get};
    let hebergeur = Router::new().route(
        "/media/42",
        get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "audio/mp4")],
                vec![0u8; 64],
            )
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let amont = tokio::spawn(async move {
        axum::serve(ecoute, hebergeur).await.unwrap();
    });

    let (state, zid) = zone_sur_appareil_sans_media().await;
    let (statut, corps) = poster(
        &state,
        &format!("/api/v1/podcasts/play/{zid}"),
        serde_json::json!({"audio_url": format!("{base}/media/42"), "title": "Épisode"}),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(types_recus(&state).await, vec!["audio/mp4".to_string()]);

    let (statut, corps) = pause(&state, zid, "fr").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["state"], "stopped", "{corps}");

    let (statut, corps) = poster(&state, &format!("/api/v1/zones/{zid}/resume"), Value::Null).await;
    assert!(
        statut.is_success(),
        "la reprise doit aboutir : {statut} {corps}"
    );
    let types = types_recus(&state).await;
    assert_eq!(
        types.len(),
        2,
        "Lecture rejoue l'épisode sur l'appareil : {types:?}"
    );
    assert_eq!(
        types[1], "audio/mp4",
        "à la reprise, l'appareil doit recevoir le type retenu au lancement, \
         pas audio/mpeg : {types:?}"
    );
    amont.abort();
}
