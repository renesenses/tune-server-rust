use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(kiosk_status))
        .route("/config", get(kiosk_config).post(set_kiosk_config))
        .route("/now-playing", get(kiosk_now_playing))
        .route("/display", get(kiosk_display_data))
        .route("/screensaver", get(kiosk_screensaver))
}

fn load_kiosk_settings(state: &AppState) -> Value {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("kiosk_config")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({
            "enabled": false,
            "zone_id": null,
            "theme": "dark",
            "font_size": "large",
            "show_queue": true,
            "artwork_size": "full",
            "screensaver_timeout_minutes": 10,
        }))
}

async fn kiosk_status(State(state): State<AppState>) -> Json<Value> {
    let config = load_kiosk_settings(&state);
    let enabled = config["enabled"].as_bool().unwrap_or(false);
    Json(json!({
        "enabled": enabled,
        "config": config,
    }))
}

async fn kiosk_config(State(state): State<AppState>) -> Json<Value> {
    Json(load_kiosk_settings(&state))
}

#[derive(Deserialize)]
struct KioskConfigBody {
    enabled: Option<bool>,
    zone_id: Option<String>,
    theme: Option<String>,
    font_size: Option<String>,
    show_queue: Option<bool>,
    artwork_size: Option<String>,
    screensaver_timeout_minutes: Option<i64>,
}

async fn set_kiosk_config(
    State(state): State<AppState>,
    Json(body): Json<KioskConfigBody>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::new(state.db.clone());
    let mut config = load_kiosk_settings(&state);
    let obj = config.as_object_mut().ok_or_else(|| AppError::internal("kiosk config is not a JSON object"))?;

    if let Some(v) = body.enabled {
        obj.insert("enabled".into(), json!(v));
    }
    if let Some(v) = &body.zone_id {
        obj.insert("zone_id".into(), json!(v));
    }
    if let Some(v) = &body.theme {
        obj.insert("theme".into(), json!(v));
    }
    if let Some(v) = &body.font_size {
        obj.insert("font_size".into(), json!(v));
    }
    if let Some(v) = body.show_queue {
        obj.insert("show_queue".into(), json!(v));
    }
    if let Some(v) = &body.artwork_size {
        obj.insert("artwork_size".into(), json!(v));
    }
    if let Some(v) = body.screensaver_timeout_minutes {
        obj.insert("screensaver_timeout_minutes".into(), json!(v));
    }

    settings
        .set("kiosk_config", &serde_json::to_string(&config)?)
        .ok();
    Ok(Json(json!({"saved": true, "config": config})))
}

#[derive(Deserialize)]
struct NowPlayingParams {
    zone_id: Option<String>,
}

async fn kiosk_now_playing(
    State(state): State<AppState>,
    Query(params): Query<NowPlayingParams>,
) -> Json<Value> {
    let config = load_kiosk_settings(&state);
    let zone_id = params
        .zone_id
        .or_else(|| config["zone_id"].as_str().map(String::from));

    // Get current playback state from the playback manager
    let playback = state.playback.clone();
    let zone_id_num = zone_id
        .as_deref()
        .and_then(|z| z.parse::<i64>().ok())
        .unwrap_or(1);
    let zone_state = playback.get_state(zone_id_num).await;

    let playing = zone_state.state == tune_core::playback::PlayState::Playing;
    Json(json!({
        "playing": playing,
        "zone_id": zone_id,
        "state": zone_state.state,
        "track": zone_state.now_playing,
        "position_ms": zone_state.position_ms,
    }))
}

async fn kiosk_display_data(
    State(state): State<AppState>,
    Query(params): Query<NowPlayingParams>,
) -> Json<Value> {
    let config = load_kiosk_settings(&state);
    let zone_id = params
        .zone_id
        .or_else(|| config["zone_id"].as_str().map(String::from));

    let playback = state.playback.clone();
    let zone_id_num = zone_id
        .as_deref()
        .and_then(|z| z.parse::<i64>().ok())
        .unwrap_or(1);
    let zone_state = playback.get_state(zone_id_num).await;

    Json(json!({
        "zone_id": zone_id,
        "theme": config["theme"],
        "font_size": config["font_size"],
        "show_queue": config["show_queue"],
        "artwork_size": config["artwork_size"],
        "now_playing": zone_state.now_playing,
        "state": zone_state.state,
        "position_ms": zone_state.position_ms,
        "queue_position": zone_state.queue_position,
        "queue_length": zone_state.queue_length,
    }))
}

#[derive(Deserialize)]
struct ScreensaverParams {
    limit: Option<i64>,
}

async fn kiosk_screensaver(
    State(state): State<AppState>,
    Query(params): Query<ScreensaverParams>,
) -> Result<Json<Value>, AppError> {
    let limit = params.limit.unwrap_or(20);
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;

    // Get random albums with artwork
    let albums: Vec<Value> = conn
        .prepare(
            "SELECT id, title, artist_name, cover_path FROM albums \
             WHERE cover_path IS NOT NULL AND cover_path != '' \
             ORDER BY RANDOM() LIMIT ?1",
        )
        .and_then(|mut stmt| {
            stmt.query_map([limit], |row| {
                Ok(json!({
                    "album_id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                    "cover_path": row.get::<_, Option<String>>(3)?,
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    Ok(Json(json!({
        "albums": albums,
        "total": albums.len(),
    })))
}
