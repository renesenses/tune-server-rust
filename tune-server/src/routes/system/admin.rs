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

/// Tail window read from the end of the log file. 1 MiB comfortably covers far
/// more than any reasonable `max_lines` of error lines while keeping the read
/// bounded regardless of how large the log has grown.
const ERROR_LOG_TAIL_BYTES: u64 = 1024 * 1024;

pub(super) async fn admin_errors(Query(q): Query<AdminErrorsQuery>) -> Json<Value> {
    let max_lines = q.lines.unwrap_or(100);

    let Ok(log_path) = std::env::var("TUNE_LOG_FILE") else {
        return admin_errors_disabled();
    };

    // Read only the tail of the log, off the async runtime. Reading the whole
    // file synchronously here (it grows to hundreds of MB on a long-running
    // server, worse under heavy random playback) blocked a Tokio worker on every
    // 5s dashboard poll and froze the UI (Jean Valjean #1096 — "F5 pour sortir").
    let result = tokio::task::spawn_blocking(move || read_error_tail(&log_path, max_lines)).await;

    match result {
        Ok(Some((recent, source))) => Json(json!({
            "errors": recent,
            "count": recent.len(),
            "source": source,
        })),
        _ => admin_errors_disabled(),
    }
}

fn admin_errors_disabled() -> Json<Value> {
    Json(json!({
        "errors": [],
        "count": 0,
        "source": null,
        "message": "Set TUNE_LOG_FILE to enable error log viewing",
    }))
}

/// Read the last `ERROR_LOG_TAIL_BYTES` of `log_path`, keep lines that look like
/// errors, and return the most recent `max_lines` of them (newest first).
/// Returns `None` if the file can't be opened/read.
fn read_error_tail(log_path: &str, max_lines: usize) -> Option<(Vec<String>, String)> {
    read_error_tail_windowed(log_path, max_lines, ERROR_LOG_TAIL_BYTES)
}

fn read_error_tail_windowed(
    log_path: &str,
    max_lines: usize,
    tail_bytes: u64,
) -> Option<(Vec<String>, String)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(log_path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);
    // If we started mid-file the first line is likely truncated — drop it.
    let body = if start > 0 {
        text.find('\n').map(|nl| &text[nl + 1..]).unwrap_or("")
    } else {
        &text
    };

    Some((filter_error_lines(body, max_lines), log_path.to_string()))
}

/// Keep lines that look like errors and return the most recent `max_lines`
/// of them, newest first.
fn filter_error_lines(body: &str, max_lines: usize) -> Vec<String> {
    body.lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.contains("error") || lower.contains("panic") || lower.contains("fatal")
        })
        .rev()
        .take(max_lines)
        .map(|s| s.to_string())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn filter_keeps_only_errors_newest_first_capped() {
        let body = "\
INFO started
ERROR disk full
INFO playing
panic at the disco
WARN nothing
FATAL meltdown";
        // newest-first, all three error-ish lines
        let out = filter_error_lines(body, 10);
        assert_eq!(
            out,
            vec!["FATAL meltdown", "panic at the disco", "ERROR disk full"]
        );
        // max_lines cap keeps the most recent ones
        let capped = filter_error_lines(body, 2);
        assert_eq!(capped, vec!["FATAL meltdown", "panic at the disco"]);
    }

    #[test]
    fn tail_window_drops_partial_first_line() {
        // Unique temp path without external crates.
        let mut path = std::env::temp_dir();
        path.push(format!("tune_admin_errors_test_{}.log", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "ERROR very-old-should-be-cut-by-window").unwrap();
        writeln!(f, "ERROR recent-one").unwrap();
        writeln!(f, "INFO tail-noise").unwrap();
        f.flush().unwrap();

        // The file is 72 bytes; a 50-byte window starts inside the first
        // ("very-old") line, so that line is partial and dropped — while the
        // whole "recent-one" line survives.
        let (lines, src) = read_error_tail_windowed(path.to_str().unwrap(), 10, 50).unwrap();
        assert_eq!(src, path.to_str().unwrap());
        assert!(
            !lines.iter().any(|l| l.contains("very-old")),
            "partial first line must be dropped, got {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("recent-one")));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_missing_file_returns_none() {
        assert!(read_error_tail("/nonexistent/tune/admin/errors.log", 100).is_none());
    }
}
