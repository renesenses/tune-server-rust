use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::error::AppError;
use crate::state::AppState;

pub(super) async fn version() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
    }))
}

pub(super) async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "components": {
            "database": true,
            "scanner": true,
            "streamer": true,
            "discovery": true
        }
    }))
}

pub(super) async fn stats(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let listens = HistoryRepo::new(state.db.clone()).count().unwrap_or(0);
    let zones = ZoneRepo::new(state.db).count().unwrap_or(0);
    let devices = state.scanner.lock().await.devices().await.len();
    let outputs = state.outputs.lock().await.list().len();

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "zones": zones,
        "devices": devices,
        "outputs": outputs,
        "server_version": tune_core::version(),
        "server_engine": "rust",
    }))
}

pub(super) async fn get_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    let defaults: Vec<(&str, Value)> = vec![
        ("api_port", json!(state.port)),
        ("stream_port", json!(state.port)),
        ("tidal_enabled", json!(true)),
        ("qobuz_enabled", json!(true)),
        ("youtube_enabled", json!(true)),
        ("spotify_enabled", json!(false)),
        ("deezer_enabled", json!(true)),
        ("amazon_music_enabled", json!(false)),
        ("discovery_enabled", json!(true)),
        ("squeezebox_enabled", json!(false)),
        ("db_engine", json!("sqlite")),
        ("db_connected", json!(true)),
        ("metadata_readonly", json!(false)),
        ("enrich_on_scan", json!(false)),
        ("resample_policy", json!("none")),
        ("audio_buffer_kb", json!(256)),
        ("prebuffer_seconds", json!(1.0)),
    ];
    for (k, v) in defaults {
        config.entry(k.to_string()).or_insert(v);
    }
    config
        .entry("server_version".to_string())
        .or_insert(json!(tune_core::version()));
    config
        .entry("server_engine".to_string())
        .or_insert(json!("rust"));
    // Ensure onboarding_completed is always present as a boolean
    let onboarding_complete = config
        .get("onboarding_complete")
        .and_then(|v| v.as_str())
        .map(|v| v == "true")
        .or_else(|| config.get("onboarding_complete").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    config
        .entry("onboarding_completed".to_string())
        .or_insert(json!(onboarding_complete));
    Json(Value::Object(config))
}

pub(super) async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| state.config.db_path.clone());
    let onboarding_completed = settings
        .get("onboarding_complete")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let theme = settings.get("theme").ok().flatten();

    Json(json!({
        "music_dirs": music_dirs,
        "db_path": db_path,
        "web_dir": state.config.web_dir,
        "artwork_dir": state.config.artwork_dir,
        "port": state.port,
        "auto_scan": state.config.auto_scan,
        "onboarding_completed": onboarding_completed,
        "server_version": tune_core::version(),
        "server_engine": "rust",
        "theme": theme,
    }))
}

#[derive(Deserialize)]
pub(super) struct ConfigPatch(pub(super) serde_json::Map<String, Value>);

pub(super) async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> Result<impl IntoResponse, AppError> {
    let settings = SettingsRepo::new(state.db);
    for (key, value) in body.0 {
        let str_val = if value.is_string() {
            value
                .as_str()
                .ok_or_else(|| AppError::bad_request("expected string"))?
                .to_string()
        } else {
            value.to_string()
        };
        if let Err(e) = settings.set(&key, &str_val) {
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
    }
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
pub(super) struct ThemeRequest {
    theme: String,
}

pub(super) async fn set_theme(
    State(state): State<AppState>,
    Json(body): Json<ThemeRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("theme", &body.theme).ok();
    Json(json!({ "theme": body.theme }))
}

pub(super) async fn get_theme(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let theme = settings.get("theme").ok().flatten();
    Json(json!({ "theme": theme }))
}

pub(super) async fn get_env() -> Json<Value> {
    let port = std::env::var("TUNE_PORT").unwrap_or_else(|_| "8085".into());
    let db = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());

    Json(json!({
        "TUNE_PORT": port,
        "TUNE_DB_PATH": db,
    }))
}

pub(super) async fn get_mode(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let mode = settings
        .get("server_mode")
        .ok()
        .flatten()
        .unwrap_or_else(|| "server".into());
    Json(json!({ "mode": mode }))
}

#[derive(Deserialize)]
pub(super) struct SetMode {
    mode: String,
}

pub(super) async fn set_mode(
    State(state): State<AppState>,
    Json(body): Json<SetMode>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("server_mode", &body.mode).ok();
    Json(json!({ "mode": body.mode }))
}

pub(super) async fn export_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    Json(Value::Object(config))
}

pub(super) async fn import_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<impl IntoResponse, AppError> {
    let settings = SettingsRepo::new(state.db);
    let mut imported = 0;
    for (key, value) in body {
        let str_val = if value.is_string() {
            value
                .as_str()
                .ok_or_else(|| AppError::bad_request("expected string"))?
                .to_string()
        } else {
            value.to_string()
        };
        if settings.set(&key, &str_val).is_ok() {
            imported += 1;
        }
    }
    Ok(Json(json!({ "imported": imported })))
}

pub(super) async fn clear_cache(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("scan_result", "{}").ok();
    Json(json!({ "cleared": true }))
}

pub(super) async fn get_music_dirs(State(state): State<AppState>) -> Json<Value> {
    let dirs = super::get_music_dirs_list(&state.db);
    Json(json!({ "dirs": dirs }))
}

#[derive(Deserialize)]
pub(super) struct BrowseDirsQuery {
    path: Option<String>,
}

pub(super) async fn browse_dirs(Query(q): Query<BrowseDirsQuery>) -> Json<Value> {
    let base = q.path.unwrap_or_else(|| {
        if cfg!(target_os = "windows") {
            "C:\\".into()
        } else {
            "/".into()
        }
    });

    let base_path = std::path::Path::new(&base);
    if !base_path.exists() || !base_path.is_dir() {
        return Json(
            json!({ "dirs": [], "parent": null, "current": base, "error": "not a directory" }),
        );
    }

    let parent = base_path.parent().map(|p| p.to_string_lossy().to_string());

    let mut dirs: Vec<Value> = Vec::new();

    // On Windows, list drives when at root
    #[cfg(target_os = "windows")]
    if base == "C:\\" || base == "\\" || base == "/" {
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if std::path::Path::new(&drive).exists() {
                dirs.push(json!({
                    "name": format!("{} Drive", letter as char),
                    "path": drive,
                    "has_children": true,
                }));
            }
        }
        return Json(json!({ "dirs": dirs, "parent": null, "current": base }));
    }

    if let Ok(entries) = std::fs::read_dir(base_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden dirs and system dirs
            if name.starts_with('.')
                || name == "$RECYCLE.BIN"
                || name == "System Volume Information"
            {
                continue;
            }
            let has_children = std::fs::read_dir(&path)
                .map(|mut rd| rd.any(|e| e.is_ok_and(|e| e.path().is_dir())))
                .unwrap_or(false);
            dirs.push(json!({
                "name": name,
                "path": path.to_string_lossy(),
                "has_children": has_children,
            }));
        }
    }

    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });

    Json(json!({
        "dirs": dirs,
        "parent": parent,
        "current": base_path.to_string_lossy(),
    }))
}

#[derive(Deserialize)]
pub(super) struct AddMusicDir {
    path: String,
}

pub(super) async fn add_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);

    if normalized.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path is empty" })),
        )
            .into_response());
    }

    let path = std::path::Path::new(&normalized);
    if !path.exists() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "directory does not exist",
                "path": normalized,
            })),
        )
            .into_response());
    }
    if !path.is_dir() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "path is not a directory",
                "path": normalized,
            })),
        )
            .into_response());
    }

    let settings = SettingsRepo::new(state.db);
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if !dirs.contains(&normalized) {
        dirs.push(normalized);
    }

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();
    Ok(Json(json!({ "dirs": dirs })).into_response())
}

pub(super) async fn remove_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Result<Json<Value>, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);
    let settings = SettingsRepo::new(state.db);
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    dirs.retain(|d| {
        let norm_d = tune_core::scanner::walker::normalize_path(d);
        norm_d != normalized
    });

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();
    Ok(Json(json!({ "dirs": dirs })))
}

pub(super) async fn restart() -> impl IntoResponse {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}
