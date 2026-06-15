use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct AdminErrorsQuery {
    lines: Option<usize>,
}

pub(super) async fn admin_errors(Query(q): Query<AdminErrorsQuery>) -> Json<Value> {
    let max_lines = q.lines.unwrap_or(100);

    // Try reading from TUNE_LOG_FILE if set
    if let Ok(log_path) = std::env::var("TUNE_LOG_FILE") {
        if let Ok(content) = std::fs::read_to_string(&log_path) {
            let all_lines: Vec<&str> = content.lines().collect();
            let error_lines: Vec<&str> = all_lines
                .iter()
                .filter(|l| {
                    let lower = l.to_lowercase();
                    lower.contains("error") || lower.contains("panic") || lower.contains("fatal")
                })
                .copied()
                .collect();
            let recent: Vec<&str> = error_lines.into_iter().rev().take(max_lines).collect();
            return Json(json!({
                "errors": recent,
                "count": recent.len(),
                "source": log_path,
            }));
        }
    }

    Json(json!({
        "errors": [],
        "count": 0,
        "source": null,
        "message": "Set TUNE_LOG_FILE to enable error log viewing",
    }))
}

pub(super) async fn admin_connections(State(state): State<AppState>) -> Json<Value> {
    let streamer_sessions = state.streamer.sessions_state();
    let active_streams = streamer_sessions.lock().await.len();
    let outputs = state.outputs.lock().await;
    let registered_outputs = outputs.list().len();

    Json(json!({
        "websocket_connections": 0,
        "active_streams": active_streams,
        "registered_outputs": registered_outputs,
    }))
}

pub(super) async fn admin_discovery(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;

    Json(json!({
        "device_count": devices.len(),
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "host": d.host,
            "type": format!("{:?}", d.device_type),
        })).collect::<Vec<_>>(),
    }))
}

pub(super) async fn admin_health(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed().as_secs();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let zone_count = ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let playback_states = state.playback.all_states().await;
    let playing = playback_states
        .iter()
        .filter(|z| z.state == tune_core::playback::PlayState::Playing)
        .count();
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    drop(outputs);
    let services = state.services.lock().await;
    let service_count = services.list().len();
    drop(services);

    Json(json!({
        "status": "ok",
        "uptime_seconds": uptime,
        "engine": "rust",
        "version": tune_core::version(),
        "database": {
            "tracks": tracks,
            "albums": albums,
            "engine": "sqlite",
        },
        "playback": {
            "zones_total": zone_count,
            "zones_playing": playing,
        },
        "outputs": output_count,
        "streaming_services": service_count,
        "scan_status": scan_status,
    }))
}

pub(super) async fn admin_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.list().unwrap_or_default();
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        result.push(json!({
            "id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "output_device_id": z.output_device_id,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "volume": if ps.volume > 0.0 { ps.volume } else { z.volume as f64 / 100.0 },
            "muted": z.muted,
            "current_track": ps.now_playing,
            "position_ms": ps.position_ms,
            "queue_length": ps.queue_length,
        }));
    }
    Json(json!(result))
}

pub(super) async fn system_peers() -> Json<Value> {
    Json(json!([]))
}

pub(super) async fn discover_servers() -> Json<Value> {
    Json(json!({ "servers": [], "message": "peer discovery not yet implemented" }))
}

pub(super) async fn listening_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let history = repo.listening_history(30).unwrap_or_default();
    let total_listens = repo.count().unwrap_or(0);
    let total_hours: f64 = history
        .iter()
        .map(|(_, _, ms)| *ms as f64 / 3_600_000.0)
        .sum();
    Json(json!({
        "total_listens": total_listens,
        "total_hours_30d": (total_hours * 100.0).round() / 100.0,
        "daily": history.iter().map(|(day, plays, ms)| json!({
            "day": day, "plays": plays, "hours": (*ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
        })).collect::<Vec<_>>(),
    }))
}
