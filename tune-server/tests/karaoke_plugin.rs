//! Karaoke plugin, from the outside: routes mounted by the host under
//! `/api/v1/ext/karaoke`, exercised end-to-end against a seeded in-memory DB.
//!
//! CRITICAL: these tests must never touch the network. `tune_core::lyrics::
//! get_lyrics` is cache-first — it consults the `lyrics_cache` table before it
//! would ever call LRCLIB. Every track here is seeded into that cache, so the
//! LRCLIB branch is never reached. (A cache *miss* would fire a real HTTP
//! request and make the test flaky/slow — hence the seeding.)
#![cfg(feature = "karaoke")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Mount only the Karaoke plugin router, wired with the state's real services —
/// the same `(name, Router<()>)` shape the host uses. `name = "karaoke"` makes
/// the host mount it at `/api/v1/ext/karaoke`.
fn app_with_karaoke(state: &AppState) -> axum::Router {
    let plugin_router = tune_karaoke::router(
        state.backend.clone(),
        state.http_client.clone(),
        state.playback.clone(),
    );
    tune_server::routes::router_with_plugins(
        state.clone(),
        vec![("karaoke".to_string(), plugin_router)],
    )
}

/// Insert a minimal track and return its id.
fn seed_track(state: &AppState, title: &str, artist: &str) -> i64 {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut track = Track::new(title.to_string());
    track.artist_name = Some(artist.to_string());
    track.duration_ms = 180_000;
    track.file_path = Some(format!("/music/{title}.flac"));
    repo.create(&track).unwrap()
}

/// Seed the lyrics cache with synced LRC text so `get_lyrics` returns from the
/// DB and never calls LRCLIB.
fn seed_lyrics_cache(state: &AppState, track_id: i64, title: &str, artist: &str, lrc: &str) {
    let sql = "INSERT OR REPLACE INTO lyrics_cache \
               (track_id, title, artist, synced_lyrics, plain_lyrics, source, fetched_at) \
               VALUES (?, ?, ?, ?, ?, 'lrclib', '2026-01-01T00:00:00Z')";
    let plain = "plain fallback";
    let params: [&dyn ToSqlValue; 5] = [&track_id, &title, &artist, &lrc, &plain];
    state.backend.execute(sql, &params).unwrap();
}

const LRC: &str = "[00:12.34] First line\n[00:15.00] Second line\n[00:30.00] Third line";

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// Mounting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_route_is_mounted_under_ext_karaoke() {
    let state = new_state();
    let app = app_with_karaoke(&state);

    let (status, body) = get_json(&app, "/api/v1/ext/karaoke/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "karaoke");
    assert_eq!(body["enabled"], true);
    assert!(body["version"].is_string(), "version should be present");
}

#[tokio::test]
async fn core_lyrics_route_is_unaffected_by_the_plugin() {
    // The plugin is additive: it must not delete or shadow the core
    // `/api/v1/lyrics/{id}` route. Seed the cache so the core handler (also
    // cache-first) answers without the network, and assert it is still mounted.
    let state = new_state();
    let track_id = seed_track(&state, "Core Song", "Core Artist");
    seed_lyrics_cache(&state, track_id, "Core Song", "Core Artist", LRC);

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/lyrics/{track_id}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "core lyrics route must still respond: {body:?}"
    );
    assert_eq!(body["track_id"], track_id);
}

// ---------------------------------------------------------------------------
// /lyrics/{track_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lyrics_returns_synced_lines_from_cache_without_network() {
    let state = new_state();
    let track_id = seed_track(&state, "Karaoke Song", "Karaoke Artist");
    seed_lyrics_cache(&state, track_id, "Karaoke Song", "Karaoke Artist", LRC);

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/ext/karaoke/lyrics/{track_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["track_id"], track_id);
    assert_eq!(body["synced"], true);
    assert_eq!(body["source"], "lrclib");
    assert!(body["error"].is_null(), "error should be null: {body:?}");

    let lines = body["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3, "expected 3 synced lines: {body:?}");
    assert_eq!(lines[0]["time_ms"], 12_340);
    assert_eq!(lines[0]["text"], "First line");
    assert_eq!(lines[1]["time_ms"], 15_000);
    assert_eq!(lines[1]["text"], "Second line");
    assert_eq!(lines[2]["time_ms"], 30_000);
    assert_eq!(lines[2]["text"], "Third line");
}

#[tokio::test]
async fn lyrics_for_missing_track_is_404() {
    let state = new_state();
    let app = app_with_karaoke(&state);
    let (status, _) = get_json(&app, "/api/v1/ext/karaoke/lyrics/99999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// /now/{zone_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn now_computes_current_line_index_from_position() {
    let state = new_state();
    let track_id = seed_track(&state, "Now Song", "Now Artist");
    seed_lyrics_cache(&state, track_id, "Now Song", "Now Artist", LRC);

    // Seed a zone playing this track at 16s — past line[1] (15s), before
    // line[2] (30s), so the active line index is 1.
    let zone_id = 7;
    let mut np = NowPlaying::from_track(
        &TrackRepo::with_backend(state.backend.clone())
            .get(track_id)
            .unwrap()
            .unwrap(),
    );
    np.track_id = Some(track_id);
    state.playback.play(zone_id, np).await;
    state.playback.update_position(zone_id, 16_000).await;

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/ext/karaoke/now/{zone_id}")).await;

    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["track_id"], track_id);
    assert_eq!(body["position_ms"], 16_000);
    assert_eq!(body["current_index"], 1, "16s → line index 1: {body:?}");
    assert!(body["error"].is_null());
    assert_eq!(body["lines"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn now_with_nothing_playing_reports_empty() {
    let state = new_state();
    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, "/api/v1/ext/karaoke/now/123").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["track_id"].is_null());
    assert_eq!(body["current_index"], -1);
}
