//! #8170 (suite de #8152 et de tune-web-client#2730) — `GET
//! /podcasts/positions` rend la position de CHAQUE épisode commencé, pas
//! seulement celle du dernier écouté du podcast : la fiche du podcast relance
//! chacun là où on l'a laissé.
//!
//! * `chaque_episode_commence_rend_sa_position_8170` : deux épisodes du même
//!   podcast, deux positions (`/home/continue-listening` n'en rend qu'une) ;
//! * `un_episode_termine_ne_rend_plus_de_position_8170` ;
//! * `la_position_de_la_zone_prime_8170` : même règle que la reprise par
//!   adresse (#8085).

use crate::state::AppState;
use axum::{body::Body, http::Request};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::listening_resume as reprises;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;

const PODCAST: &str = "Émission témoin 8170";
const EP1: &str = "http://127.0.0.1:9/episode-1-8170.mp3";
const EP2: &str = "http://127.0.0.1:9/episode-2-8170.mp3";
const DUREE_MS: i64 = 1_800_000;

fn episode(url: &str, titre: &str) -> NowPlaying {
    NowPlaying {
        title: titre.into(),
        artist_name: Some(PODCAST.into()),
        album_title: Some(PODCAST.into()),
        duration_ms: DUREE_MS,
        source: "podcast".into(),
        source_id: Some(url.into()),
        ..Default::default()
    }
}

fn etat() -> AppState {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async { AppState::new(":memory:", 0, Default::default()).unwrap() })
}

async fn positions(state: &AppState, requete: &str) -> Vec<(String, i64)> {
    let rep = crate::routes::router(state.clone())
        .oneshot(Request::get(requete).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(rep.status().is_success(), "{requete} : {}", rep.status());
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&octets).unwrap();
    let mut lus: Vec<(String, i64)> = v["positions"]
        .as_array()
        .unwrap_or_else(|| panic!("positions absentes : {v}"))
        .iter()
        .map(|p| {
            assert_eq!(p["duration_ms"], DUREE_MS, "{p}");
            (
                p["audio_url"].as_str().unwrap().to_string(),
                p["position_ms"].as_i64().unwrap(),
            )
        })
        .collect();
    lus.sort();
    lus
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chaque_episode_commence_rend_sa_position_8170() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — 8170", None, None)
        .unwrap();
    reprises::podcast_started_in_context(
        &state.backend,
        zone,
        &episode(EP1, "Un"),
        120_000,
        Some("emission-8170"),
    )
    .unwrap();
    reprises::podcast_started_in_context(
        &state.backend,
        zone,
        &episode(EP2, "Deux"),
        600_000,
        Some("emission-8170"),
    )
    .unwrap();
    assert_eq!(
        positions(&state, "/api/v1/podcasts/positions").await,
        vec![(EP1.to_string(), 120_000), (EP2.to_string(), 600_000)],
        "les DEUX épisodes commencés du podcast, chacun à sa position"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_episode_termine_ne_rend_plus_de_position_8170() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — 8170", None, None)
        .unwrap();
    reprises::podcast_started_in_context(&state.backend, zone, &episode(EP1, "Un"), 120_000, None)
        .unwrap();
    reprises::podcast_started_in_context(
        &state.backend,
        zone,
        &episode(EP2, "Deux"),
        600_000,
        None,
    )
    .unwrap();
    reprises::podcast_termine(&state.backend, zone, EP1).unwrap();
    assert_eq!(
        positions(
            &state,
            &format!("/api/v1/podcasts/positions?zone_id={zone}")
        )
        .await,
        vec![(EP2.to_string(), 600_000)]
    );
}

#[test]
fn la_position_de_la_zone_prime_8170() {
    let state = etat();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let a = zones.create("A — 8170", None, None).unwrap();
    let b = zones.create("B — 8170", None, None).unwrap();
    reprises::podcast_started_in_context(&state.backend, a, &episode(EP1, "Un"), 100_000, None)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    reprises::podcast_started_in_context(&state.backend, b, &episode(EP1, "Un"), 200_000, None)
        .unwrap();
    let position = |zone: Option<i64>| {
        reprises::positions_des_episodes(&state.backend, zone)
            .unwrap()
            .into_iter()
            .map(|p| p.position_ms)
            .collect::<Vec<_>>()
    };
    assert_eq!(position(Some(a)), vec![100_000], "celle de la zone d'abord");
    assert_eq!(position(Some(b)), vec![200_000]);
    assert_eq!(position(None), vec![200_000], "sinon la plus récente");
}
