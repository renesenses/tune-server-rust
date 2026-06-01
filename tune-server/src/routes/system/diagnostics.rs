use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

pub(super) async fn diagnostics(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let db_version = migrations::current_version(&state.db).unwrap_or(0);
    let music_dirs = super::get_music_dirs_list(&state.db);
    let ffmpeg = tune_core::audio::pipeline::find_ffmpeg();
    let uptime_secs = state.started_at.elapsed().as_secs();

    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "pid": std::process::id(),
        "uptime_seconds": uptime_secs,
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "db": {
            "engine": "sqlite",
            "migration_version": db_version,
        },
        "music_dirs": music_dirs,
        "tracks_count": tracks,
        "albums_count": albums,
        "artists_count": artists,
        "ffmpeg_path": ffmpeg,
        "ffmpeg_available": ffmpeg.is_some(),
        "rust_engines": {
            "available": true,
            "version": tune_core::version(),
            "metadata_engine": "lofty",
            "discovery_engine": "mdns-sd + socket2",
            "scanner_engine": "walkdir + rayon",
            "db_engine": "rusqlite",
        },
    }))
}

pub(super) async fn diagnostics_bundle(State(state): State<AppState>) -> Json<Value> {
    diagnostics(State(state)).await
}

pub(super) async fn diagnostics_network(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    Json(json!({
        "discovered_devices": devices.len(),
        "registered_outputs": output_count,
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "host": d.host,
            "type": format!("{:?}", d.device_type),
        })).collect::<Vec<_>>(),
    }))
}

pub(super) async fn health_monitor(State(state): State<AppState>) -> Json<Value> {
    let report = state.health_monitor.run_checks().await;
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let settings = SettingsRepo::new(state.db);
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    Json(json!({
        "status": report.status,
        "uptime_seconds": report.uptime_seconds,
        "tracks": tracks,
        "scan_status": scan_status,
        "engine": "rust",
        "checks": report.checks,
        "alerts": report.alerts,
    }))
}

pub(super) async fn health_alerts(State(state): State<AppState>) -> Json<Value> {
    let alerts = state.health_monitor.alerts().await;
    Json(json!({ "alerts": alerts }))
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    lines: Option<usize>,
}

pub(super) async fn logs(Query(q): Query<LogsQuery>) -> Json<Value> {
    let max_lines = q.lines.unwrap_or(100);

    // Try log file first
    let log_path = std::env::var("TUNE_LOG_FILE").unwrap_or_else(|_| {
        if cfg!(target_os = "windows") {
            let appdata =
                std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
            format!("{appdata}\\TuneServer\\tune-server.log")
        } else {
            "/var/log/tune-server.log".into()
        }
    });

    // Try reading log file
    if let Ok(content) = std::fs::read_to_string(&log_path) {
        let lines: Vec<&str> = content.lines().rev().take(max_lines).collect();
        let lines: Vec<&str> = lines.into_iter().rev().collect();
        return Json(json!({
            "logs": lines.join("\n"),
            "lines": lines.len(),
            "source": "file",
            "path": log_path,
        }));
    }

    // Try journalctl on Linux
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("journalctl")
            .args([
                "-u",
                "tune-server",
                "-n",
                &max_lines.to_string(),
                "--no-pager",
                "-o",
                "short-iso",
            ])
            .output()
        {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout);
                let count = text.lines().count();
                return Json(json!({
                    "logs": text,
                    "lines": count,
                    "source": "journalctl",
                }));
            }
        }
    }

    // Fallback: return in-memory ring buffer if available, or empty
    Json(json!({
        "logs": "No log file found. Launch Tune from a terminal to see logs in real-time.\nChecked: ".to_owned() + &log_path,
        "lines": 0,
        "source": "none",
    }))
}

/// Generate a bug report with comprehensive diagnostic data.
/// Returns JSON that can also be rendered as markdown by the client.
pub(super) async fn generate_bug_report(State(state): State<AppState>) -> Json<Value> {
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let uptime_secs = state.started_at.elapsed().as_secs();
    let db_version = migrations::current_version(&state.db).unwrap_or(0);
    let settings = SettingsRepo::new(state.db.clone());
    let music_dirs = super::get_music_dirs_list(&state.db);
    let ffmpeg = tune_core::audio::pipeline::find_ffmpeg();
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());

    // Zones
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let zone_count = zone_repo.count().unwrap_or(0);
    let zones: Vec<Value> = zone_repo
        .list()
        .unwrap_or_default()
        .iter()
        .map(|z| json!({ "id": z.id, "name": z.name, "output_type": z.output_type }))
        .collect();

    // Streaming services status
    let registry = state.services.lock().await;
    let service_status = registry.status_all().await;
    drop(registry);

    // Discovered devices
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;
    drop(scanner);
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    drop(outputs);

    let uptime_str = format!(
        "{}d {}h {}m {}s",
        uptime_secs / 86400,
        (uptime_secs % 86400) / 3600,
        (uptime_secs % 3600) / 60,
        uptime_secs % 60,
    );

    // Build markdown text
    let mut md = String::new();
    md.push_str(&format!("# Tune Bug Report\n\n"));
    md.push_str(&format!(
        "**Version**: {} (engine: rust)\n",
        tune_core::version()
    ));
    md.push_str(&format!(
        "**Platform**: {} ({})\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str(&format!("**Uptime**: {uptime_str}\n"));
    md.push_str(&format!("**PID**: {}\n\n", std::process::id()));

    md.push_str("## Library\n");
    md.push_str(&format!("- Tracks: {tracks}\n"));
    md.push_str(&format!("- Albums: {albums}\n"));
    md.push_str(&format!("- Artists: {artists}\n"));
    md.push_str(&format!("- Music dirs: {}\n", music_dirs.join(", ")));
    md.push_str(&format!("- Scan status: {scan_status}\n\n"));

    md.push_str(&format!("## Zones ({zone_count})\n"));
    for z in &zones {
        md.push_str(&format!(
            "- {} ({})\n",
            z["name"].as_str().unwrap_or("?"),
            z["output_type"].as_str().unwrap_or("?")
        ));
    }
    md.push_str("\n");

    md.push_str("## Streaming Services\n");
    for s in &service_status {
        let auth = if s["authenticated"].as_bool().unwrap_or(false) {
            "authenticated"
        } else {
            "not authenticated"
        };
        let enabled = if s["enabled"].as_bool().unwrap_or(false) {
            "enabled"
        } else {
            "disabled"
        };
        md.push_str(&format!(
            "- {}: {}, {}\n",
            s["name"].as_str().unwrap_or("?"),
            enabled,
            auth
        ));
    }
    md.push_str("\n");

    md.push_str(&format!("## Network\n"));
    md.push_str(&format!("- Discovered devices: {}\n", devices.len()));
    md.push_str(&format!("- Registered outputs: {output_count}\n"));
    md.push_str(&format!(
        "- FFmpeg: {}\n\n",
        ffmpeg.as_deref().unwrap_or("not found")
    ));

    md.push_str(&format!("## Database\n"));
    md.push_str(&format!("- Engine: sqlite\n"));
    md.push_str(&format!("- Migration version: {db_version}\n"));

    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_seconds": uptime_secs,
        "uptime": uptime_str,
        "pid": std::process::id(),
        "library": {
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            "music_dirs": music_dirs,
            "scan_status": scan_status,
        },
        "zones": {
            "count": zone_count,
            "items": zones,
        },
        "streaming_services": service_status,
        "network": {
            "discovered_devices": devices.len(),
            "registered_outputs": output_count,
        },
        "ffmpeg": ffmpeg,
        "database": {
            "engine": "sqlite",
            "migration_version": db_version,
        },
        "markdown": md,
    }))
}

pub(super) async fn audio_check() -> Json<Value> {
    let ffmpeg_path = tune_core::audio::pipeline::find_ffmpeg();
    let ffprobe = if ffmpeg_path.is_some() {
        // If ffmpeg is found, ffprobe is likely available too
        which_cmd("ffprobe")
    } else {
        None
    };

    let formats = if ffmpeg_path.is_some() {
        vec![
            "flac", "wav", "aiff", "mp3", "aac", "ogg", "opus", "alac", "dsd", "wma",
        ]
    } else {
        vec![]
    };

    Json(json!({
        "ffmpeg_available": ffmpeg_path.is_some(),
        "ffmpeg_path": ffmpeg_path,
        "ffprobe_available": ffprobe.is_some(),
        "ffprobe_path": ffprobe,
        "supported_formats": formats,
        "lofty_available": true,
        "engine": "rust",
    }))
}

fn which_cmd(name: &str) -> Option<String> {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Anonymous telemetry snapshot — returns what would be sent if telemetry
/// is enabled. No data leaves the server unless the user explicitly opts in.
pub(super) async fn telemetry_snapshot(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let enabled = settings.get("telemetry_enabled").ok().flatten().as_deref() == Some("true");
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let zone_count = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone())
        .count()
        .unwrap_or(0);
    let uptime = state.started_at.elapsed().as_secs();

    Json(json!({
        "enabled": enabled,
        "payload": {
            "version": tune_core::version(),
            "engine": "rust",
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "uptime_seconds": uptime,
            "tracks": tracks,
            "albums": albums,
            "artists": artists,
            "zones": zone_count,
        }
    }))
}

pub(super) async fn telemetry_toggle(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let settings = SettingsRepo::new(state.db);
    let _ = settings.set("telemetry_enabled", if enabled { "true" } else { "false" });
    Json(json!({ "enabled": enabled }))
}
