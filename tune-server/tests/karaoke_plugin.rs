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

// ── #2997 : le décalage de paroles de la ZONE est enfin appliqué ───────────
//
// `zones.lyrics_offset_ms` était stocké, exposé et borné à ±60 s sans qu'une
// seule ligne ne s'en trouve décalée. `/now/{zone_id}` est la seule surface du
// serveur qui puisse l'appliquer : elle connaît la zone, et sa réponse — qui
// porte `position_ms` — n'a jamais été partageable entre zones.
//
// Deux zones, même piste, même position : seul le décalage les distingue.

/// Crée une zone, lui pose un décalage de paroles, la met en lecture sur
/// `track_id` à `position_ms`, puis rend l'id de la zone.
async fn zone_playing_at(
    state: &AppState,
    name: &str,
    track_id: i64,
    offset_ms: i32,
    position_ms: i64,
) -> i64 {
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_id = zones.create(name, Some("local"), None).unwrap();
    zones.update_lyrics_offset_ms(zone_id, offset_ms).unwrap();

    let mut np = NowPlaying::from_track(
        &TrackRepo::with_backend(state.backend.clone())
            .get(track_id)
            .unwrap()
            .unwrap(),
    );
    np.track_id = Some(track_id);
    state.playback.play(zone_id, np).await;
    state.playback.update_position(zone_id, position_ms).await;
    zone_id
}

#[tokio::test]
async fn now_applique_le_decalage_de_paroles_de_la_zone() {
    let state = new_state();
    let track_id = seed_track(&state, "Décalée", "Artiste");
    seed_lyrics_cache(&state, track_id, "Décalée", "Artiste", LRC);

    // Position 16 s. Sans décalage, l'index actif est 1 (ligne à 15 s).
    // Avec +3 s de décalage — paroles RETARDÉES — on lit les paroles comme
    // si l'on était à 13 s : la ligne à 15 s n'est pas encore atteinte, la
    // ligne active redevient la 0 (12,34 s).
    let zone_id = zone_playing_at(&state, "Salon retardé", track_id, 3_000, 16_000).await;

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/ext/karaoke/now/{zone_id}")).await;

    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(
        body["lyrics_offset_ms"], 3_000,
        "la réponse doit porter le décalage appliqué : {body:?}"
    );
    assert_eq!(
        body["position_ms"], 16_000,
        "la position reste BRUTE — le contrat existant ne bouge pas : {body:?}"
    );
    assert_eq!(
        body["current_index"], 0,
        "16 s moins 3 s de décalage → ligne 0, pas la 1 : {body:?}"
    );
    // Les horodatages des lignes ne sont pas touchés : seul l'index l'est.
    assert_eq!(body["lines"][0]["time_ms"], 12_340, "body: {body:?}");
    assert_eq!(body["lines"][1]["time_ms"], 15_000, "body: {body:?}");
}

#[tokio::test]
async fn un_decalage_negatif_avance_les_paroles() {
    let state = new_state();
    let track_id = seed_track(&state, "Avancée", "Artiste");
    seed_lyrics_cache(&state, track_id, "Avancée", "Artiste", LRC);

    // Position 28 s, décalage -3 s : on lit comme si l'on était à 31 s, donc
    // la ligne à 30 s est déjà active (index 2) alors qu'elle ne le serait
    // pas sans décalage.
    let zone_id = zone_playing_at(&state, "Salon avancé", track_id, -3_000, 28_000).await;

    let app = app_with_karaoke(&state);
    let (_, body) = get_json(&app, &format!("/api/v1/ext/karaoke/now/{zone_id}")).await;

    assert_eq!(
        body["current_index"], 2,
        "28 s plus 3 s d'avance → ligne 2 (sans décalage ce serait 1) : {body:?}"
    );
    assert_eq!(body["lyrics_offset_ms"], -3_000, "body: {body:?}");
}

#[tokio::test]
async fn temoin_un_decalage_de_zero_rend_exactement_l_index_d_origine() {
    // TÉMOIN ANTI-RÉGRESSION — **vert avant comme après le correctif**.
    //
    // Une zone sans décalage est le cas de TOUTES les zones existantes : la
    // réponse doit être exactement celle d'avant. D'où l'absence délibérée
    // d'assertion sur `lyrics_offset_ms` ici : ce champ est nouveau, l'exiger
    // ferait échouer ce témoin sur la version d'avant et il ne témoignerait
    // plus de rien. Ce qui est vérifié, c'est ce qui ne DOIT PAS bouger —
    // l'index actif, la position brute, les horodatages des lignes.
    let state = new_state();
    let track_id = seed_track(&state, "Témoin", "Artiste");
    seed_lyrics_cache(&state, track_id, "Témoin", "Artiste", LRC);

    let zone_id = zone_playing_at(&state, "Salon témoin", track_id, 0, 16_000).await;

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/ext/karaoke/now/{zone_id}")).await;

    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["position_ms"], 16_000, "body: {body:?}");
    assert_eq!(
        body["current_index"], 1,
        "décalage nul ⇒ index d'origine : {body:?}"
    );
    let lines = body["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["time_ms"], 12_340, "body: {body:?}");
    assert_eq!(lines[1]["time_ms"], 15_000, "body: {body:?}");
    assert_eq!(lines[2]["time_ms"], 30_000, "body: {body:?}");
}

#[tokio::test]
async fn now_avec_zone_a_l_arret_porte_quand_meme_le_decalage() {
    // Le réglage doit être lisible même quand rien ne joue : sinon un client
    // ne peut pas afficher le décalage effectif d'une zone au repos.
    let state = new_state();
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_id = zones.create("Zone muette", Some("local"), None).unwrap();
    zones.update_lyrics_offset_ms(zone_id, 1_500).unwrap();

    let app = app_with_karaoke(&state);
    let (status, body) = get_json(&app, &format!("/api/v1/ext/karaoke/now/{zone_id}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["lyrics_offset_ms"], 1_500, "body: {body:?}");
    assert_eq!(body["current_index"], -1);
    assert_eq!(body["error"], "nothing playing");
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
