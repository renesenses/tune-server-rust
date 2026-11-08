//! #8152 — un épisode repris sur une zone DLNA repartait du début (.18, le
//! 08/11, Eversolo DMP-A8) : le `Seek` de reprise partait 2 ms après le
//! `Play`, 700 ms avant que le renderer ouvre le flux, qui le lisait alors
//! depuis l'octet 0.
//!
//! Aucun son : sorties factices (`MockOutput`), épisode sur le port 9 local
//! (discard). Le renderer DLNA factice reste en `Transitioning` après le
//! `Play`, comme l'Eversolo, jusqu'à ce que le banc le fasse démarrer.
use crate::state::AppState;
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::db::listening_resume as reprises;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::TransportState;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;

const APPAREIL: &str = "dlna:uuid-eversolo-8152";
const EPISODE: &str = "http://127.0.0.1:9/episode-8152.m4a";
const DUREE_MS: i64 = 3_471_000;

async fn banc(output_type: &str, appareil: &str) -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin 8152", Some(output_type), Some(appareil))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "DMP-A8")
            .with_type(output_type)
            .with_lecture_en_transition(),
    ));
    (state, zone)
}

fn episode() -> NowPlaying {
    NowPlaying {
        title: "Entretien témoin 8152".into(),
        artist_name: Some("Certains l'aiment Fip".into()),
        album_title: Some("Certains l'aiment Fip".into()),
        duration_ms: DUREE_MS,
        source: "podcast".into(),
        source_id: Some(EPISODE.into()),
        ..Default::default()
    }
}

async fn poster(state: &AppState, uri: String, corps: Value) -> (axum::http::StatusCode, Value) {
    let rep = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Le geste de « Récemment écoutés » : la source et l'adresse seules.
async fn relancer_depuis_l_accueil(state: &AppState, zone: i64) {
    let (statut, v) = poster(
        state,
        format!("/api/v1/zones/{zone}/play"),
        json!({ "source": "podcast", "source_id": EPISODE }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
}

async fn mock<R>(state: &AppState, appareil: &str, f: impl FnOnce(&MockOutput) -> R) -> R {
    let arc = { state.outputs.lock().await.get(appareil) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    f(guard.as_any().downcast_ref::<MockOutput>().expect("mock"))
}

async fn demarrer_le_renderer(state: &AppState, appareil: &str) {
    let arc = { state.outputs.lock().await.get(appareil) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock")
        .set_state(TransportState::Playing)
        .await;
}

async fn seeks_apres(state: &AppState, appareil: &str, delai: Duration) -> Vec<u64> {
    let fin = std::time::Instant::now() + delai;
    loop {
        let seeks = mock(state, appareil, |m| m.seek_calls()).await;
        if !seeks.is_empty() || std::time::Instant::now() >= fin {
            return seeks;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8152_dlna_le_seek_attend_la_lecture_puis_ramene_a_590_s() {
    let (state, zone) = banc("dlna", APPAREIL).await;
    reprises::podcast_started_in_context(&state.backend, zone, &episode(), 600_000, None).unwrap();

    relancer_depuis_l_accueil(&state, zone).await;

    // Le renderer n'a pas encore ouvert le flux : un Seek maintenant serait
    // acquitté puis perdu (il lirait depuis l'octet 0).
    tokio::time::sleep(Duration::from_millis(600)).await;
    let avant = mock(&state, APPAREIL, |m| m.seek_calls()).await;
    assert!(
        avant.is_empty(),
        "#8152 : Seek envoyé avant que le renderer soit en lecture ({avant:?}) — \
         l'Eversolo l'acquitte puis relit l'épisode depuis le début"
    );
    assert_eq!(
        state.playback.get_state(zone).await.position_ms,
        590_000,
        "la position publique suit tout de suite"
    );

    demarrer_le_renderer(&state, APPAREIL).await;
    let seeks = seeks_apres(&state, APPAREIL, Duration::from_secs(3)).await;
    assert_eq!(
        seeks,
        vec![590_000],
        "600 s moins le recul par défaut (10 s)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8152_position_nulle_aucun_seek() {
    let (state, zone) = banc("dlna", APPAREIL).await;
    reprises::podcast_started_in_context(&state.backend, zone, &episode(), 0, None).unwrap();

    relancer_depuis_l_accueil(&state, zone).await;
    demarrer_le_renderer(&state, APPAREIL).await;

    let seeks = seeks_apres(&state, APPAREIL, Duration::from_millis(800)).await;
    assert!(seeks.is_empty(), "position nulle : du début, {seeks:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8152_episode_termine_repart_du_debut() {
    let (state, zone) = banc("dlna", APPAREIL).await;
    let (statut, v) = poster(
        &state,
        format!("/api/v1/podcasts/play/{zone}"),
        json!({
            "audio_url": EPISODE,
            "title": "Entretien témoin 8152",
            "podcast_name": "Certains l'aiment Fip",
            "duration_ms": DUREE_MS,
            "seek_ms": DUREE_MS - 5_000,
        }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {v}");
    demarrer_le_renderer(&state, APPAREIL).await;

    let seeks = seeks_apres(&state, APPAREIL, Duration::from_millis(800)).await;
    assert!(seeks.is_empty(), "épisode terminé : du début, {seeks:?}");
}

/// Sortie locale : la reprise passe toujours par `seek` de l'orchestrateur
/// (recréation du flux à la position), sans Seek détaché vers la sortie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8152_sortie_locale_inchangee() {
    const LOCALE: &str = "local:temoin-8152";
    let (state, zone) = banc("local", LOCALE).await;
    let (statut, v) = poster(
        &state,
        format!("/api/v1/podcasts/play/{zone}"),
        json!({
            "audio_url": EPISODE,
            "title": "Entretien témoin 8152",
            "podcast_name": "Certains l'aiment Fip",
            "duration_ms": DUREE_MS,
            "seek_ms": 590_000,
        }),
    )
    .await;
    // Pas de mandataire pour une sortie locale : elle ouvre l'adresse.
    assert!(
        state
            .playback
            .get_state(zone)
            .await
            .now_playing
            .and_then(|np| np.stream_id)
            .is_none()
            || !statut.is_success(),
        "{statut} {v}"
    );
    demarrer_le_renderer(&state, LOCALE).await;
    let seeks = seeks_apres(&state, LOCALE, Duration::from_millis(800)).await;
    assert!(
        seeks.is_empty(),
        "sortie locale : aucun Seek détaché vers la sortie, {seeks:?} ({statut} {v})"
    );
}
