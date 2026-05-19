use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::migrations;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version", get(version))
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/config", get(get_config).patch(update_config))
        .route("/scan", post(trigger_scan))
        .route("/scan/status", get(scan_status))
        .route("/scan/cancel", post(scan_cancel))
        .route("/restart", post(restart))
        .route("/database/status", get(database_status))
        .route("/database/optimize", post(database_optimize))
        .route("/music-dirs", get(get_music_dirs))
        .route("/music-dirs/add", post(add_music_dir))
        .route("/env", get(get_env))
}

async fn version() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
    }))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn stats(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let listens = HistoryRepo::new(state.db).count().unwrap_or(0);

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
    }))
}

async fn get_config(State(state): State<AppState>) -> Json<Value> {
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

#[derive(Deserialize)]
struct ConfigPatch(serde_json::Map<String, Value>);

async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    for (key, value) in body.0 {
        let str_val = if value.is_string() {
            value.as_str().unwrap().to_string()
        } else {
            value.to_string()
        };
        if let Err(e) = settings.set(&key, &str_val) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn trigger_scan(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("scan_status", "scanning").ok();
    settings.set("scan_started_at", &chrono_now()).ok();

    let db = state.db.clone();
    tokio::spawn(async move {
        let music_dirs = get_music_dirs_list(&db);
        if music_dirs.is_empty() {
            SettingsRepo::new(db).set("scan_status", "idle").ok();
            return;
        }

        let files = tune_core::scanner::walker::list_audio_files(&music_dirs);
        let (scanned, scan_stats) = tune_core::scanner::walker::scan_files_parallel(&files, true, None);

        let track_repo = tune_core::db::track_repo::TrackRepo::new(db.clone());
        let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(db.clone());
        let album_repo = tune_core::db::album_repo::AlbumRepo::new(db.clone());

        let mut inserted = 0i64;
        let mut updated = 0i64;

        for sf in &scanned {
            if let Some(ref meta) = sf.metadata {
                let artist = artist_repo
                    .get_or_create(
                        meta.artist.as_deref().unwrap_or("Unknown Artist"),
                        meta.musicbrainz_artist_id.as_deref(),
                        meta.album_artist_sort.as_deref(),
                    )
                    .ok();

                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if let Some(ref album_title) = meta.album {
                    album_repo
                        .get_or_create(album_title, artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
                        .ok()
                } else {
                    None
                };

                let album_id = album.as_ref().and_then(|a| a.id);

                let existing = track_repo.get_by_path(&sf.path).ok().flatten();
                if existing.is_some() {
                    updated += 1;
                    continue;
                }

                let mut track = tune_core::db::models::Track::new(
                    meta.title.clone().unwrap_or_else(|| {
                        std::path::Path::new(&sf.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    }),
                );
                track.album_id = album_id;
                track.artist_id = artist_id;
                track.artist_name = meta.artist.clone();
                track.album_title = meta.album.clone();
                track.disc_number = meta.disc_number.unwrap_or(1) as i32;
                track.track_number = meta.track_number.unwrap_or(0) as i32;
                track.duration_ms = meta.duration_ms.unwrap_or(0) as i64;
                track.file_path = Some(sf.path.clone());
                track.format = meta.format.clone();
                track.sample_rate = meta.sample_rate.map(|s| s as i32);
                track.bit_depth = meta.bit_depth.map(|b| b as i32);
                track.channels = meta.channels.unwrap_or(2) as i32;
                track.file_size = Some(sf.file_size as i64);
                track.file_mtime = Some(sf.mtime as f64);
                track.audio_hash = sf.audio_hash.clone();
                track.genre = meta.genre.clone();
                track.composer = meta.credits.iter()
                    .find(|c| c.role == "composer")
                    .map(|c| c.name.clone());
                track.year = meta.year.map(|y| y as i32);
                track.bpm = meta.bpm.map(|b| b as f64);
                track.label = meta.label.clone();
                track.isrc = meta.isrc.clone();
                track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();

                if track_repo.create(&track).is_ok() {
                    inserted += 1;
                }
            }
        }

        for album in album_repo.list(99999, 0).unwrap_or_default() {
            if let Some(id) = album.id {
                album_repo.update_track_count(id).ok();
            }
        }

        let settings = SettingsRepo::new(db);
        settings.set("scan_status", "idle").ok();
        settings
            .set(
                "scan_result",
                &json!({
                    "total_files": scan_stats.total_files,
                    "metadata_ok": scan_stats.metadata_ok,
                    "metadata_failed": scan_stats.metadata_failed,
                    "inserted": inserted,
                    "updated": updated,
                })
                .to_string(),
            )
            .ok();
    });

    (StatusCode::ACCEPTED, Json(json!({ "status": "scanning" })))
}

async fn scan_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let status = settings.get("scan_status").ok().flatten().unwrap_or_else(|| "idle".into());
    let result = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Json(json!({
        "status": status,
        "result": result,
    }))
}

async fn scan_cancel(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    settings.set("scan_status", "idle").ok();
    StatusCode::NO_CONTENT
}

async fn restart() -> impl IntoResponse {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}

async fn database_status(State(state): State<AppState>) -> Json<Value> {
    let version = migrations::current_version(&state.db).unwrap_or(0);
    let latest = migrations::latest_version();
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db).count().unwrap_or(0);

    Json(json!({
        "engine": "sqlite",
        "migration_version": version,
        "latest_version": latest,
        "up_to_date": version >= latest,
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
    }))
}

async fn database_optimize(State(state): State<AppState>) -> impl IntoResponse {
    let start = Instant::now();
    match state.db.execute_batch("PRAGMA optimize; VACUUM; ANALYZE;") {
        Ok(_) => {
            let ms = start.elapsed().as_millis();
            Json(json!({ "status": "ok", "duration_ms": ms })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_music_dirs(State(state): State<AppState>) -> Json<Value> {
    let dirs = get_music_dirs_list(&state.db);
    Json(json!({ "dirs": dirs }))
}

#[derive(Deserialize)]
struct AddMusicDir {
    path: String,
}

async fn add_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if !dirs.contains(&body.path) {
        dirs.push(body.path);
    }

    settings.set("music_dirs", &serde_json::to_string(&dirs).unwrap()).ok();
    Json(json!({ "dirs": dirs }))
}

async fn get_env() -> Json<Value> {
    let port = std::env::var("TUNE_PORT").unwrap_or_else(|_| "8085".into());
    let db = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());

    Json(json!({
        "TUNE_PORT": port,
        "TUNE_DB_PATH": db,
    }))
}

fn get_music_dirs_list(db: &tune_core::db::sqlite::SqliteDb) -> Vec<String> {
    SettingsRepo::new(db.clone())
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{now}")
}
