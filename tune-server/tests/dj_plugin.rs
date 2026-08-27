//! DJ mode, from the outside, now that it is a native plugin (P5, #917).
//!
//! Only compiled with `--features dj`. It exercises the *real* wiring: the
//! compiled-in registration arm in `plugins::register_builtin_plugins`
//! constructs `tune_dj::DjPlugin`, `plugins::init` sets it up, and the router
//! it contributes is mounted under `/api/v1/ext/dj` — the same path any plugin
//! gets, derived from `name()`, not chosen by DJ.
//!
//! The stub endpoints (status, crossfade, …) assert the JSON contract is
//! unchanged from the old core `routes/dj.rs`. `waveform`/`analyze` prove the
//! plugin has full native audio access: they decode a real WAV through
//! `tune_core::audio::decode::decode_to_pcm`.
#![cfg(feature = "dj")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::models::Track;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Build the app with the DJ plugin loaded through the real registration path.
async fn app_with_dj(state: &AppState) -> axum::Router {
    use_scratch_plugin_data_dir();

    // DJ est OPT-IN depuis le suivi de #917 : `default_enabled()` renvoie
    // false, et `setup_all` le laisse dormant tant que
    // `plugin_dj_installed=true` n'est pas posé — c'est ce qui le fait
    // apparaître dans le gestionnaire de greffons au lieu de tourner d'office.
    // Sans cette ligne, sur une base `:memory:` neuve, DJ reste dormant et
    // `init` ne renvoie aucun routeur : ces six tests échouaient depuis, et
    // personne ne le voyait parce que la CI ne compile pas `--features dj`.
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_dj_installed", "true")
        .expect("marquer DJ installé");

    let routers = tune_server::plugins::init(state, "http://127.0.0.1:0", vec![]).await;

    // On cherche DJ par son nom plutôt que d'exiger un routeur unique : le
    // jeu de fonctionnalités livré compile aussi karaoke et plugins-wasm, qui
    // en contribuent d'autres. Un test sur DJ ne doit pas échouer parce qu'un
    // greffon voisin existe.
    let dj = routers
        .iter()
        .find(|(name, _)| name == "dj")
        .map(|(name, _)| name.clone());
    assert_eq!(
        dj.as_deref(),
        Some("dj"),
        "le greffon dj doit contribuer un routeur monté sous son name()"
    );

    tune_server::routes::router_with_plugins(state.clone(), routers)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    body_json(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    body_json(app, req).await
}

async fn body_json(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// A minimal mono 16-bit PCM WAV with a 440 Hz sine, `seconds` long at 44.1 kHz.
/// Long enough that `analyze` gets its 4+ energy windows.
fn write_wav(path: &std::path::Path, seconds: u32) {
    let sample_rate: u32 = 44_100;
    let n = sample_rate * seconds;
    let mut samples: Vec<u8> = Vec::with_capacity(n as usize * 2);
    for i in 0..n {
        let t = i as f32 / sample_rate as f32;
        let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
        let v = (s * 20_000.0) as i16;
        samples.extend_from_slice(&v.to_le_bytes());
    }
    let data_len = samples.len() as u32;

    let mut wav: Vec<u8> = Vec::with_capacity(44 + samples.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&samples);

    std::fs::write(path, wav).unwrap();
}

/// Insert a track pointing at `file_path`, return its id.
fn seed_track(state: &AppState, file_path: &str) -> i64 {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut track = Track::new("Test Tone".to_string());
    track.file_path = Some(file_path.to_string());
    track.format = Some("wav".to_string());
    repo.create(&track).unwrap()
}

// ---------------------------------------------------------------------------
// Mounting + stub contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dj_routes_mount_under_ext_dj() {
    let state = new_state();
    let app = app_with_dj(&state).await;

    let (status, body) = get_json(&app, "/api/v1/ext/dj/status/1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["zone_id"], 1);
    assert_eq!(body["dj_mode"], false);
    assert_eq!(body["crossfader"], 0.5);
    assert_eq!(body["deck_a"]["loaded"], false);

    // The old core path is gone.
    let (gone, _) = get_json(&app, "/api/v1/dj/status/1").await;
    assert_eq!(gone, StatusCode::NOT_FOUND, "/dj must no longer be served");
}

#[tokio::test]
async fn dj_crossfade_returns_expected_json() {
    let state = new_state();
    let app = app_with_dj(&state).await;

    let (status, body) = post_json(
        &app,
        "/api/v1/ext/dj/crossfade",
        serde_json::json!({"zone_id": 7, "duration_ms": 3000}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["zone_id"], 7);
    assert_eq!(body["crossfade_started"], true);
    assert_eq!(body["duration_ms"], 3000);

    // Crossfader position is clamped.
    let (status, body) = post_json(
        &app,
        "/api/v1/ext/dj/crossfader",
        serde_json::json!({"zone_id": 7, "position": 1.7}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["crossfader"], 1.0);
}

#[tokio::test]
async fn dj_enable_persists_and_status_reflects_it() {
    let state = new_state();
    let app = app_with_dj(&state).await;

    let (status, body) = post_json(&app, "/api/v1/ext/dj/enable/5", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dj_mode"], true);

    let (_, body) = get_json(&app, "/api/v1/ext/dj/status/5").await;
    assert_eq!(body["dj_mode"], true, "enable must persist to settings");
}

// ---------------------------------------------------------------------------
// Native audio access — the reason DJ is a native plugin, not WASM
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dj_waveform_decodes_a_real_file() {
    let state = new_state();
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    write_wav(&wav, 2);
    let id = seed_track(&state, wav.to_str().unwrap());

    let app = app_with_dj(&state).await;
    let (status, body) = get_json(&app, &format!("/api/v1/ext/dj/waveform/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["track_id"], id);
    assert!(body["error"].is_null(), "decode must succeed, got {body:?}");
    assert!(
        body["points"].as_u64().unwrap() > 0,
        "a decoded tone must yield waveform points"
    );
    assert!(body["waveform"].is_array());
}

#[tokio::test]
async fn dj_analyze_decodes_a_real_file() {
    let state = new_state();
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("tone.wav");
    write_wav(&wav, 2);
    let id = seed_track(&state, wav.to_str().unwrap());

    let app = app_with_dj(&state).await;
    let (status, body) =
        post_json(&app, &format!("/api/v1/ext/dj/analyze/{id}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["track_id"], id);
    // A 2-second tone decodes and yields a duration; BPM may be null (a pure
    // sine has no beats), but the native decode path must have run — proven by
    // the absence of the "native decode failed" error and a real duration.
    assert!(body["error"].is_null(), "decode must succeed, got {body:?}");
    assert_eq!(body["duration_s"], 2.0);
    assert!(body["beats_detected"].is_number());
}

#[tokio::test]
async fn dj_waveform_missing_track_is_404() {
    let state = new_state();
    let app = app_with_dj(&state).await;
    let (status, _) = get_json(&app, "/api/v1/ext/dj/waveform/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
