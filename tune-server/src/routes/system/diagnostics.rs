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
    let uptime_secs = state.started_at.elapsed().as_secs();

    // Zone count
    let zone_count = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone())
        .count()
        .unwrap_or(0);

    // Discovered devices grouped by type
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;
    drop(scanner);
    let mut devices_by_type: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for d in &devices {
        devices_by_type
            .entry(d.device_type.to_string())
            .or_default()
            .push(d.name.clone());
    }

    // Connectors (streaming services)
    let registry = state.services.lock().await;
    let connectors: Vec<String> = registry.list();
    drop(registry);

    // Audio outputs
    let audio_backend_pref = &state.config.local_audio_backend;
    let (audio_outputs, audio_backend_name, asio_avail) = {
        #[cfg(feature = "local-audio")]
        {
            let devs: Vec<String> =
                tune_core::outputs::local::list_audio_devices_with_backend(audio_backend_pref)
                    .iter()
                    .map(|d| d.name.clone())
                    .collect();
            let name = tune_core::outputs::local::active_backend_name(audio_backend_pref);
            let asio = tune_core::outputs::local::asio_available();
            (devs, name, asio)
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = audio_backend_pref;
            (Vec::<String>::new(), "none", false)
        }
    };

    // Scan status
    let settings = SettingsRepo::new(state.db.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let scan_result: Option<serde_json::Value> = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Memory RSS
    let rss_mb = get_rss_mb();

    // DB backend
    let db_backend = settings
        .get("db_engine")
        .ok()
        .flatten()
        .unwrap_or_else(|| "sqlite".into());

    Json(json!({
        "server_version": tune_core::version(),
        "rust_version": tune_core::rustc_version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "uptime_seconds": uptime_secs,
        "memory_rss_mb": rss_mb,
        "db_backend": db_backend,
        "active_zones": zone_count,
        "discovered_devices": devices_by_type,
        "connectors": connectors,
        "audio_outputs_available": audio_outputs,
        "audio_backend": audio_backend_name,
        "asio_available": asio_avail,
        "scan_status": {
            "status": scan_status,
            "tracks": tracks,
            "albums": albums,
            "last_result": scan_result,
        },
        "features": tune_core::enabled_features(),
        // Legacy fields kept for backward compatibility
        "engine": "rust",
        "platform": std::env::consts::OS,
        "pid": std::process::id(),
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "db": {
            "engine": db_backend,
            "migration_version": db_version,
        },
        "music_dirs": music_dirs,
        "tracks_count": tracks,
        "albums_count": albums,
        "artists_count": artists,
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

/// Read process RSS in megabytes. Returns None on unsupported platforms.
fn get_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * 4096 / 1024 / 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id();
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8(o.stdout)
                    .ok()?
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .map(|kb| kb / 1024)
            })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None::<u64>
    }
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

pub(super) async fn diagnostics_oaat(State(state): State<AppState>) -> Json<Value> {
    let outputs = state.outputs.lock().await;
    let mut endpoints = Vec::new();
    for id in outputs.list() {
        if let Some(output) = outputs.get(&id) {
            let output = output.lock().await;
            if let Some(diag) = output.diagnostics_json() {
                endpoints.push(diag);
            }
        }
    }
    Json(json!({
        "oaat_endpoints": endpoints,
        "count": endpoints.len(),
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

    // Memory RSS
    let rss_mb = {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|pages| pages * 4096 / 1024 / 1024)
        }
        #[cfg(target_os = "macos")]
        {
            let pid = std::process::id();
            std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()?
                        .trim()
                        .parse::<u64>()
                        .ok()
                        .map(|kb| kb / 1024)
                })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None::<u64>
        }
    };

    // OAAT diagnostics
    let oaat_endpoints: Vec<Value> = {
        let outputs = state.outputs.lock().await;
        outputs
            .list()
            .iter()
            .filter_map(|id| {
                let output = outputs.get(id)?;
                let output = output.try_lock().ok()?;
                output.diagnostics_json()
            })
            .collect()
    };

    // Build markdown text
    let mut md = String::new();
    md.push_str("# Tune Bug Report\n\n");
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
    md.push_str(&format!("**PID**: {}\n", std::process::id()));
    if let Some(rss) = rss_mb {
        md.push_str(&format!("**Memory**: {rss} MB RSS\n"));
    }
    md.push('\n');

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
    md.push('\n');

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
    md.push('\n');

    md.push_str("## Network\n");
    md.push_str(&format!("- Discovered devices: {}\n", devices.len()));
    md.push_str(&format!("- Registered outputs: {output_count}\n"));
    md.push('\n');

    if !oaat_endpoints.is_empty() {
        md.push_str(&format!("## OAAT Endpoints ({})\n", oaat_endpoints.len()));
        for ep in &oaat_endpoints {
            md.push_str(&format!(
                "- {} ({}): connected={}, packets={}, format={}\n",
                ep["name"].as_str().unwrap_or("?"),
                ep["host"].as_str().unwrap_or("?"),
                ep["connected"].as_bool().unwrap_or(false),
                ep["packets_sent"].as_u64().unwrap_or(0),
                ep["format"].as_str().unwrap_or("?"),
            ));
            if ep["stall_detected"].as_bool().unwrap_or(false) {
                md.push_str("  **⚠ STALL DETECTED**\n");
            }
        }
        md.push('\n');
    }

    md.push_str("## Database\n");
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
        "rss_mb": rss_mb,
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
        "oaat_endpoints": oaat_endpoints,
        "database": {
            "engine": "sqlite",
            "migration_version": db_version,
        },
        "markdown": md,
    }))
}

/// Returns the bug report as raw markdown (text/markdown) for direct forum paste.
pub(super) async fn bug_report_markdown(
    State(state): State<AppState>,
) -> (
    axum::http::StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    String,
) {
    let Json(report) = generate_bug_report(State(state)).await;
    let md = report["markdown"].as_str().unwrap_or("").to_string();
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        md,
    )
}

pub(super) async fn audio_check() -> Json<Value> {
    let formats = vec![
        "flac", "wav", "aiff", "mp3", "aac", "ogg", "opus", "alac", "dsd", "wavpack", "ape",
    ];

    Json(json!({
        "native_engine": true,
        "supported_formats": formats,
        "lofty_available": true,
        "engine": "rust",
    }))
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

pub(super) async fn api_stats(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    Json(serde_json::to_value(stats).unwrap_or_default())
}

pub(super) async fn api_insights(State(state): State<AppState>) -> Json<Value> {
    let stats = state.api_analytics.stats();
    let mut issues: Vec<Value> = Vec::new();

    // High error rate
    if stats.error_rate_pct > 5.0 {
        issues.push(json!({
            "severity": "warning",
            "type": "high_error_rate",
            "message": format!("API error rate is {:.1}% (threshold: 5%)", stats.error_rate_pct),
        }));
    }

    // Slow endpoints (P95 > 500ms)
    for ep in &stats.slowest_endpoints {
        if ep.p95_latency_ms > 500 {
            issues.push(json!({
                "severity": "warning",
                "type": "slow_endpoint",
                "endpoint": ep.endpoint,
                "p95_ms": ep.p95_latency_ms,
                "message": format!("{} P95 latency {}ms (threshold: 500ms)", ep.endpoint, ep.p95_latency_ms),
            }));
        }
    }

    // Zone poller issues
    let metrics = state.poller_metrics.lock().await;
    for (zone_id, m) in metrics.iter() {
        if m.total_polls > 10 && m.total_errors > 0 {
            let err_pct = m.total_errors as f64 / m.total_polls as f64 * 100.0;
            if err_pct > 10.0 {
                issues.push(json!({
                    "severity": "error",
                    "type": "zone_poll_failures",
                    "zone_id": zone_id,
                    "error_rate_pct": (err_pct * 10.0).round() / 10.0,
                    "message": format!("Zone {} has {:.0}% poll error rate", zone_id, err_pct),
                }));
            }
        }
        if m.max_latency_ms > 2000 {
            issues.push(json!({
                "severity": "warning",
                "type": "zone_high_latency",
                "zone_id": zone_id,
                "max_latency_ms": m.max_latency_ms,
                "message": format!("Zone {} max latency {}ms", zone_id, m.max_latency_ms),
            }));
        }
    }
    drop(metrics);

    let status = if issues.iter().any(|i| i["severity"] == "error") {
        "degraded"
    } else if issues.is_empty() {
        "healthy"
    } else {
        "warning"
    };

    Json(json!({
        "status": status,
        "issues": issues,
        "total_issues": issues.len(),
        "api_requests_analyzed": stats.total_requests,
    }))
}

pub(super) async fn api_docs() -> Json<Value> {
    let routes = vec![
        // System
        ("GET", "/system/version", "Server version and engine"),
        ("GET", "/system/health", "Health check"),
        (
            "GET",
            "/system/stats",
            "Library statistics (tracks, albums, artists, zones)",
        ),
        ("GET", "/system/diagnostics", "Full diagnostic report"),
        ("GET", "/system/changelog", "Version changelog"),
        (
            "GET",
            "/system/api-stats",
            "Per-endpoint latency and error analytics",
        ),
        (
            "GET",
            "/system/api-docs",
            "This endpoint — API documentation",
        ),
        ("GET", "/system/telemetry", "Telemetry snapshot (opt-in)"),
        ("POST", "/system/scan", "Trigger library scan"),
        ("GET", "/system/scan/status", "Scan progress"),
        ("GET", "/system/logs", "Server logs"),
        ("GET", "/system/backups", "List backups"),
        ("POST", "/system/backups", "Create backup"),
        ("POST", "/system/backups/encrypt", "Create encrypted backup"),
        ("POST", "/system/import/roon", "Import from Roon"),
        ("POST", "/system/import/jriver", "Import from JRiver XML"),
        ("POST", "/system/import/plex", "Import from Plex"),
        // Library
        (
            "GET",
            "/library/albums",
            "List albums (paginated, filterable)",
        ),
        (
            "GET",
            "/library/albums/grouped",
            "Albums grouped by release (deluxe/remastered)",
        ),
        ("GET", "/library/albums/{id}", "Album details"),
        ("GET", "/library/albums/{id}/tracks", "Album tracks"),
        (
            "GET",
            "/library/albums/{id}/completeness",
            "Album track completeness check",
        ),
        ("GET", "/library/artists", "List artists"),
        (
            "GET",
            "/library/artists/{id}/timeline",
            "Artist discography with gaps",
        ),
        ("GET", "/library/tracks", "List tracks (paginated)"),
        (
            "GET",
            "/library/tracks/{id}/waveform",
            "Track waveform (200-point amplitude)",
        ),
        (
            "GET",
            "/library/tracks/{id}/synced-lyrics",
            "Synchronized lyrics (.lrc)",
        ),
        (
            "GET",
            "/library/tracks/{id}/source-links",
            "Cross-service matches",
        ),
        (
            "POST",
            "/library/identify",
            "Identify track via AcoustID fingerprint",
        ),
        (
            "GET",
            "/library/duplicates",
            "Duplicate tracks (hash + fingerprint + metadata)",
        ),
        (
            "GET",
            "/library/stats/completeness",
            "Library health score (A-F grade)",
        ),
        ("GET", "/library/genre-tree", "Hierarchical genre tree"),
        ("GET", "/search", "Federated search (local + streaming)"),
        // Zones & Playback
        ("GET", "/zones", "List zones"),
        ("POST", "/zones", "Create zone"),
        (
            "GET",
            "/zones/{id}/status",
            "Zone playback status + credits",
        ),
        (
            "GET",
            "/zones/{id}/network-health",
            "Zone network quality metrics",
        ),
        ("GET", "/zones/sync-status", "All zones with poller metrics"),
        ("POST", "/zones/{id}/play", "Play track/album/playlist"),
        ("POST", "/zones/{id}/pause", "Pause"),
        ("POST", "/zones/{id}/next", "Next track"),
        ("POST", "/zones/{id}/sleep", "Sleep timer with fade"),
        ("GET", "/zones/{id}/dsp", "Zone DSP/EQ config"),
        // Streaming
        (
            "GET",
            "/streaming/services",
            "List streaming services status",
        ),
        (
            "GET",
            "/streaming/compare",
            "Compare search across services",
        ),
        (
            "GET",
            "/streaming/{service}/search",
            "Search a streaming service",
        ),
        // Playlists
        ("GET", "/playlists", "List playlists"),
        ("POST", "/playlists", "Create playlist"),
        (
            "GET",
            "/playlists/{id}/export",
            "Export (format=m3u|json|csv|xspf)",
        ),
        // Radio & DJ
        ("GET", "/radio/auto", "Auto-DJ playlist from seed track"),
        ("GET", "/radios", "List radio stations"),
        // Dashboard
        ("GET", "/dashboard/stats", "Listening dashboard"),
        ("GET", "/dashboard/wrapped", "Year-in-review Wrapped stats"),
        ("GET", "/dashboard/top-artists", "Top artists"),
        ("GET", "/dashboard/genre-breakdown", "Genre distribution"),
        // Party
        ("POST", "/party/rooms", "Create collaborative room"),
        ("GET", "/party/rooms", "List rooms"),
        // Other
        (
            "POST",
            "/voice-search",
            "Voice search via Whisper transcription",
        ),
        (
            "GET",
            "/demo/library",
            "Read-only library browse (demo mode)",
        ),
    ];

    let endpoints: Vec<Value> = routes.iter().map(|(method, path, desc)| {
        json!({"method": method, "path": format!("/api/v1{path}"), "description": desc})
    }).collect();

    Json(json!({
        "version": tune_core::version(),
        "total_endpoints": endpoints.len(),
        "endpoints": endpoints,
    }))
}

/// List ASIO audio devices (Windows-only, requires `asio` feature).
pub(super) async fn asio_devices(State(_state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let devices = tokio::task::spawn_blocking(tune_core::outputs::local::list_asio_devices)
            .await
            .unwrap_or_default();
        let count = devices.len();
        Json(json!({
            "devices": devices,
            "asio_available": tune_core::outputs::local::asio_available(),
            "count": count,
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        Json(json!({
            "devices": [],
            "asio_available": false,
            "count": 0,
        }))
    }
}
