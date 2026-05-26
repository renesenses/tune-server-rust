use std::time::Instant;

use axum::extract::{Path, Query, State};
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
        .route("/settings", get(get_settings))
        .route("/scan", post(trigger_scan))
        .route("/scan/status", get(scan_status))
        .route("/scan/cancel", post(scan_cancel))
        .route("/restart", post(restart))
        .route("/database/status", get(database_status))
        .route("/database/optimize", post(database_optimize))
        .route("/music-dirs", get(get_music_dirs))
        .route("/music-dirs/add", post(add_music_dir))
        .route("/music-dirs/remove", post(remove_music_dir))
        .route("/env", get(get_env))
        .route("/diagnostics", get(diagnostics))
        .route("/cleanup", post(cleanup))
        .route("/logs", get(logs))
        .route("/backups", get(list_backups).post(create_backup))
        .route("/backups/{filename}/restore", post(restore_backup))
        .route("/database/export", get(export_database))
        .route("/update/check", get(update_check))
        .route("/changelog", get(changelog))
        .route("/peers", get(system_peers))
        .route("/scan/schedule", get(scan_schedule).post(set_scan_schedule))
        .route("/diagnostics/bundle", get(diagnostics_bundle))
        .route("/diagnostics/network", get(diagnostics_network))
        .route("/health/monitor", get(health_monitor))
        .route("/health/alerts", get(health_alerts))
        .route("/clear-cache", post(clear_cache))
        .route("/mode", get(get_mode).post(set_mode))
        .route("/stats/listening", get(listening_stats))
        .route("/discover-servers", get(discover_servers))
        .route("/config/export", get(export_config))
        .route("/config/import", post(import_config))
        // Import routes
        .route("/import/roon", post(import_roon))
        .route("/import/plex", post(import_plex))
        .route("/import/playlists", post(import_playlists_file))
        .route("/import/status/{task_id}", get(import_status))
        // Database engine routes
        .route("/database/test-connection", post(test_db_connection))
        .route("/database/migrate", post(migrate_database))
        // Remote/proxy mode routes
        .route("/remote/config", get(get_remote_config).post(set_remote_config))
        .route("/remote/status", get(remote_status))
        // Admin routes
        .route("/admin/errors", get(admin_errors))
        .route("/admin/connections", get(admin_connections))
        .route("/admin/discovery", get(admin_discovery))
}

async fn version() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
    }))
}

async fn health() -> Json<Value> {
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
        "server_version": tune_core::version(),
        "server_engine": "rust",
    }))
}

async fn get_config(State(state): State<AppState>) -> Json<Value> {
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
    config.entry("server_version".to_string()).or_insert(json!(tune_core::version()));
    config.entry("server_engine".to_string()).or_insert(json!("rust"));
    Json(Value::Object(config))
}

async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| state.config.db_path.clone());

    Json(json!({
        "music_dirs": music_dirs,
        "db_path": db_path,
        "web_dir": state.config.web_dir,
        "artwork_dir": state.config.artwork_dir,
        "port": state.port,
        "auto_scan": state.config.auto_scan,
        "server_version": tune_core::version(),
        "server_engine": "rust",
    }))
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

        let cache_dir = super::library::artwork_cache_dir();
        let mut albums_with_cover: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut inserted = 0i64;
        let mut updated = 0i64;
        let mut skipped = 0i64;
        let mut artwork_extracted = 0i64;

        // Load all existing local tracks in one query for efficient change detection
        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

        for sf in &scanned {
            if let Some(ref meta) = sf.metadata {
                // Determine if this is a compilation (Various Artists)
                let is_compilation = meta.compilation
                    || meta.album_artist.as_deref().map(|s| s.to_lowercase())
                        .map(|s| s == "various artists" || s == "various" || s == "va" || s == "compilations")
                        .unwrap_or(false);

                // Album artist: use album_artist tag, fall back to track artist
                let album_artist_name = meta.album_artist.as_deref()
                    .unwrap_or_else(|| meta.artist.as_deref().unwrap_or("Unknown Artist"));

                // Track artist: always from track-level artist tag
                let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

                // For the album, use album_artist (so compilations group under "Various Artists")
                let album_artist_entry = artist_repo
                    .get_or_create(
                        album_artist_name,
                        if is_compilation { None } else { meta.musicbrainz_artist_id.as_deref() },
                        meta.album_artist_sort.as_deref(),
                    )
                    .ok();
                let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

                // For the track, use track-level artist (important for compilations)
                let track_artist = if is_compilation && track_artist_name != album_artist_name {
                    artist_repo
                        .get_or_create(
                            track_artist_name,
                            meta.musicbrainz_artist_id.as_deref(),
                            None,
                        )
                        .ok()
                } else {
                    album_artist_entry.clone()
                };
                let artist_id = track_artist.as_ref().and_then(|a| a.id);

                let album = if let Some(ref album_title) = meta.album {
                    album_repo
                        .get_or_create(album_title, album_artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
                        .ok()
                } else {
                    None
                };

                let album_id = album.as_ref().and_then(|a| a.id);

                if let Some(aid) = album_id
                    && !albums_with_cover.contains(&aid)
                    && let Some(hash) = tune_core::artwork::get_or_extract(
                        std::path::Path::new(&sf.path),
                        &cache_dir,
                    ) {
                        album_repo.update_cover_path(aid, &hash).ok();
                        albums_with_cover.insert(aid);
                        artwork_extracted += 1;
                    }

                // Issue 3: Check for artist image if not already set
                if let Some(ref art) = track_artist {
                    if art.image_path.is_none() {
                        if let Some(parent) = std::path::Path::new(&sf.path).parent() {
                            for name in &["artist.jpg", "artist.png", "Artist.jpg", "Artist.png"] {
                                let candidate = parent.join(name);
                                if candidate.exists() {
                                    let hash = tune_core::artwork::artwork_hash(&candidate.to_string_lossy());
                                    let ext = candidate.extension().and_then(|e| e.to_str()).unwrap_or("jpg");
                                    if let Ok(data) = std::fs::read(&candidate) {
                                        tune_core::artwork::save_to_cache(&data, &cache_dir, &hash, ext);
                                    }
                                    let mut updated_artist = art.clone();
                                    updated_artist.image_path = Some(hash);
                                    updated_artist.image_source = Some("local".to_string());
                                    artist_repo.update(&updated_artist).ok();
                                    break;
                                }
                            }
                        }
                    }
                }

                // Build the track fields shared between insert and update
                let title = meta.title.clone().unwrap_or_else(|| {
                    std::path::Path::new(&sf.path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                });

                // Check if this file already exists in the DB
                if let Some(&(existing_id, existing_mtime, existing_size)) = existing_tracks.get(&sf.path) {
                    // File exists — check if it has changed (different mtime or size)
                    let file_changed = existing_mtime.map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
                        || existing_size.map_or(true, |s| s != sf.file_size as i64);

                    if !file_changed {
                        skipped += 1;
                        continue;
                    }

                    // File changed — update metadata
                    let mut track = tune_core::db::models::Track::new(title);
                    track.id = Some(existing_id);
                    track.album_id = album_id;
                    track.artist_id = artist_id;
                    track.artist_name = Some(track_artist_name.to_string());
                    track.album_artist = meta.album_artist.clone();
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
                    track.bpm = meta.bpm;
                    track.label = meta.label.clone();
                    track.isrc = meta.isrc.clone();
                    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();

                    if track_repo.update(&track).is_ok() {
                        updated += 1;
                    }
                    continue;
                }

                // New file — insert
                let mut track = tune_core::db::models::Track::new(title);
                track.album_id = album_id;
                track.artist_id = artist_id;
                track.artist_name = Some(track_artist_name.to_string());
                track.album_artist = meta.album_artist.clone();
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
                track.bpm = meta.bpm;
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
                album_repo.update_quality_from_tracks(id).ok();
            }
        }

        let settings = SettingsRepo::new(db.clone());
        settings.set("scan_status", "idle").ok();
        tracing::info!(
            scanned = scan_stats.total_files,
            inserted,
            updated,
            skipped,
            artwork = artwork_extracted,
            "scan_and_import_complete"
        );

        settings
            .set(
                "scan_result",
                &json!({
                    "total_files": scan_stats.total_files,
                    "metadata_ok": scan_stats.metadata_ok,
                    "metadata_failed": scan_stats.metadata_failed,
                    "inserted": inserted,
                    "updated": updated,
                    "skipped": skipped,
                    "artwork_extracted": artwork_extracted,
                })
                .to_string(),
            )
            .ok();

        // Launch batch artwork enrichment as a background task
        // This fetches covers from MusicBrainz Cover Art Archive for albums
        // that don't have embedded cover art.
        let enrich_db = db.clone();
        tokio::spawn(async move {
            tune_core::artwork::batch_enrich_artwork(enrich_db, cache_dir).await;
        });
    });

    (StatusCode::ACCEPTED, Json(json!({ "status": "scanning" })))
}

async fn scan_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let status = settings.get("scan_status").ok().flatten().unwrap_or_else(|| "idle".into());
    let scanning = status == "scanning";
    let result = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Json(json!({
        "status": status,
        "scanning": scanning,
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

async fn remove_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    dirs.retain(|d| d != &body.path);

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

async fn diagnostics(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let db_version = migrations::current_version(&state.db).unwrap_or(0);
    let music_dirs = get_music_dirs_list(&state.db);
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

async fn cleanup(State(state): State<AppState>) -> Json<Value> {
    let album_repo = AlbumRepo::new(state.db.clone());

    let merged_albums = merge_duplicate_albums(&state.db);
    let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).deduplicate().unwrap_or(0);

    let db_optimized = state
        .db
        .execute_batch("PRAGMA optimize; ANALYZE;")
        .is_ok();

    Json(json!({
        "duplicate_albums_merged": merged_albums,
        "orphan_albums_deleted": orphan_albums,
        "duplicate_tracks_removed": tracks,
        "db_optimized": db_optimized,
    }))
}

fn merge_duplicate_albums(db: &tune_core::db::sqlite::SqliteDb) -> i64 {
    let conn = db.connection().lock().unwrap();
    let dupes: Vec<(String, String)> = conn
        .prepare("SELECT title, GROUP_CONCAT(id) FROM albums GROUP BY title HAVING COUNT(id) > 1")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 { continue; }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        for &aid in &ids {
            let cnt: i64 = conn.query_row(
                "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                rusqlite::params![aid], |r| r.get(0),
            ).unwrap_or(0);
            if cnt > best_count { best_count = cnt; best_id = aid; }
        }
        for &aid in &ids {
            if aid != best_id {
                conn.execute("UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    rusqlite::params![best_id, aid]).ok();
                conn.execute("DELETE FROM albums WHERE id = ?", rusqlite::params![aid]).ok();
                deleted += 1;
            }
        }
    }
    conn.execute_batch(
        "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
    ).ok();
    deleted
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
}

async fn logs(Query(q): Query<LogsQuery>) -> Json<Value> {
    let _lines = q.lines.unwrap_or(100);
    Json(json!({
        "logs": "log retrieval not yet implemented (journalctl/file)",
        "lines": 0,
    }))
}

async fn list_backups() -> Json<Value> {
    let backup_dir = backup_dir_path();
    let mut items = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&backup_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "db").unwrap_or(false) {
                let meta = std::fs::metadata(&path).ok();
                items.push(json!({
                    "filename": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
                    "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    "created_at": meta.and_then(|m| m.created().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                }));
            }
        }
    }

    items.sort_by(|a, b| {
        b.get("created_at").and_then(|v| v.as_u64())
            .cmp(&a.get("created_at").and_then(|v| v.as_u64()))
    });

    Json(json!(items))
}

async fn create_backup(State(state): State<AppState>) -> impl IntoResponse {
    let backup_dir = backup_dir_path();
    std::fs::create_dir_all(&backup_dir).ok();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let filename = format!("tune_backup_{now}.db");
    let dest = format!("{backup_dir}/{filename}");

    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot backup in-memory database").into_response();
    }

    state.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

    match std::fs::copy(&db_path, &dest) {
        Ok(size) => Json(json!({
            "filename": filename,
            "size": size,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("backup failed: {e}")).into_response(),
    }
}

async fn restore_backup(
    State(_state): State<AppState>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let backup_dir = backup_dir_path();
    let source = format!("{backup_dir}/{filename}");

    if !std::path::Path::new(&source).exists() {
        return (StatusCode::NOT_FOUND, "backup not found").into_response();
    }

    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot restore to in-memory database").into_response();
    }

    match std::fs::copy(&source, &db_path) {
        Ok(_) => Json(json!({
            "restored": true,
            "filename": filename,
            "message": "restart required to apply",
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("restore failed: {e}")).into_response(),
    }
}

async fn export_database(State(state): State<AppState>) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot export in-memory database").into_response();
    }

    state.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();

    match tokio::fs::read(&db_path).await {
        Ok(bytes) => {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("Content-Type", axum::http::HeaderValue::from_static("application/x-sqlite3"));
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str("attachment; filename=\"tune_server.db\"").unwrap(),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("export failed: {e}")).into_response(),
    }
}

async fn update_check() -> Json<Value> {
    Json(json!({
        "current_version": tune_core::version(),
        "latest_version": null,
        "update_available": false,
        "engine": "rust",
        "message": "auto-update not yet implemented",
    }))
}

async fn system_peers() -> Json<Value> {
    Json(json!([]))
}

async fn changelog() -> Json<Value> {
    Json(json!({ "entries": [], "version": tune_core::version() }))
}

async fn scan_schedule(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let time = settings.get("scan_schedule_time").ok().flatten().unwrap_or_else(|| "03:00".into());
    let enabled = settings.get("scan_schedule_enabled").ok().flatten().map(|v| v == "true").unwrap_or(false);
    Json(json!({ "enabled": enabled, "time": time }))
}

#[derive(Deserialize)]
struct ScanScheduleReq {
    enabled: bool,
    time: Option<String>,
}

async fn set_scan_schedule(
    State(state): State<AppState>,
    Json(body): Json<ScanScheduleReq>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("scan_schedule_enabled", if body.enabled { "true" } else { "false" }).ok();
    if let Some(ref t) = body.time {
        settings.set("scan_schedule_time", t).ok();
    }
    Json(json!({ "enabled": body.enabled, "time": body.time }))
}

async fn diagnostics_bundle(State(state): State<AppState>) -> Json<Value> {
    diagnostics(State(state)).await
}

async fn diagnostics_network(State(state): State<AppState>) -> Json<Value> {
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

async fn health_monitor(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed().as_secs();
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let settings = SettingsRepo::new(state.db);
    let scan_status = settings.get("scan_status").ok().flatten().unwrap_or_else(|| "idle".into());
    Json(json!({
        "status": "ok",
        "uptime_seconds": uptime,
        "tracks": tracks,
        "scan_status": scan_status,
        "engine": "rust",
        "memory_mb": null,
    }))
}

async fn health_alerts() -> Json<Value> {
    Json(json!({ "alerts": [] }))
}

async fn clear_cache(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("scan_result", "{}").ok();
    Json(json!({ "cleared": true }))
}

async fn get_mode(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let mode = settings.get("server_mode").ok().flatten().unwrap_or_else(|| "server".into());
    Json(json!({ "mode": mode }))
}

#[derive(Deserialize)]
struct SetMode {
    mode: String,
}

async fn set_mode(
    State(state): State<AppState>,
    Json(body): Json<SetMode>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("server_mode", &body.mode).ok();
    Json(json!({ "mode": body.mode }))
}

async fn listening_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::new(state.db);
    let history = repo.listening_history(30).unwrap_or_default();
    let total_listens = repo.count().unwrap_or(0);
    let total_hours: f64 = history.iter().map(|(_, _, ms)| *ms as f64 / 3_600_000.0).sum();
    Json(json!({
        "total_listens": total_listens,
        "total_hours_30d": (total_hours * 100.0).round() / 100.0,
        "daily": history.iter().map(|(day, plays, ms)| json!({
            "day": day, "plays": plays, "hours": (*ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
        })).collect::<Vec<_>>(),
    }))
}

async fn discover_servers() -> Json<Value> {
    Json(json!({ "servers": [], "message": "peer discovery not yet implemented" }))
}

async fn export_config(State(state): State<AppState>) -> Json<Value> {
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

async fn import_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let mut imported = 0;
    for (key, value) in body {
        let str_val = if value.is_string() {
            value.as_str().unwrap().to_string()
        } else {
            value.to_string()
        };
        if settings.set(&key, &str_val).is_ok() {
            imported += 1;
        }
    }
    Json(json!({ "imported": imported }))
}

fn backup_dir_path() -> String {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    let parent = std::path::Path::new(&db_path)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    format!("{}/backups", parent.display())
}

// ---------------------------------------------------------------------------
// Import: Roon / Plex / Playlists
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImportTrackEntry {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    file_path: Option<String>,
    duration_ms: Option<i64>,
    track_number: Option<i32>,
    genre: Option<String>,
}

#[derive(Deserialize)]
struct ImportRoonRequest {
    roon_db_path: Option<String>,
    data: Option<Vec<ImportTrackEntry>>,
}

async fn import_roon(
    State(state): State<AppState>,
    Json(body): Json<ImportRoonRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let db = state.db.clone();
    let tid = task_id.clone();

    // Store initial task status
    let settings = SettingsRepo::new(db.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0, "skipped": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        let track_repo = TrackRepo::new(db.clone());
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());
        let settings = SettingsRepo::new(db.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // --- Path A: direct JSON data ---
        if let Some(entries) = body.data {
            for entry in &entries {
                // Skip if file_path exists and already in DB
                if let Some(ref fp) = entry.file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist_name = entry.artist.as_deref().unwrap_or("Unknown Artist");
                let artist = artist_repo.get_or_create(artist_name, None, None).ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if let Some(ref album_title) = entry.album {
                    album_repo
                        .get_or_create(album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut track = tune_core::db::models::Track::new(entry.title.clone());
                track.artist_id = artist_id;
                track.artist_name = entry.artist.clone();
                track.album_id = album_id;
                track.album_title = entry.album.clone();
                track.duration_ms = entry.duration_ms.unwrap_or(0);
                track.track_number = entry.track_number.unwrap_or(0);
                track.genre = entry.genre.clone();
                track.file_path = entry.file_path.clone();
                track.source = "roon_import".to_string();

                match track_repo.create(&track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(format!("{}: {e}", entry.title)),
                }
            }
        }
        // --- Path B: SQLite database path ---
        else if let Some(ref db_path) = body.roon_db_path {
            match rusqlite::Connection::open_with_flags(
                db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                Ok(conn) => {
                    // Roon's DB schema is proprietary; try common table/column names
                    let query = "SELECT title, artist, album, path, duration, track_number, genre \
                                 FROM tracks";
                    match conn.prepare(query) {
                        Ok(mut stmt) => {
                            let rows = stmt.query_map([], |row| {
                                Ok((
                                    row.get::<_, String>(0).unwrap_or_default(),
                                    row.get::<_, Option<String>>(1).ok().flatten(),
                                    row.get::<_, Option<String>>(2).ok().flatten(),
                                    row.get::<_, Option<String>>(3).ok().flatten(),
                                    row.get::<_, Option<i64>>(4).ok().flatten(),
                                    row.get::<_, Option<i32>>(5).ok().flatten(),
                                    row.get::<_, Option<String>>(6).ok().flatten(),
                                ))
                            });
                            if let Ok(rows) = rows {
                                for row in rows.flatten() {
                                    let (title, artist, album, file_path, duration, track_num, genre) = row;

                                    if let Some(ref fp) = file_path {
                                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                                            skipped += 1;
                                            continue;
                                        }
                                    }

                                    let artist_name = artist.as_deref().unwrap_or("Unknown Artist");
                                    let art = artist_repo.get_or_create(artist_name, None, None).ok();
                                    let artist_id = art.as_ref().and_then(|a| a.id);

                                    let alb = if let Some(ref album_title) = album {
                                        album_repo
                                            .get_or_create(album_title, artist_id.unwrap_or(0), None)
                                            .ok()
                                    } else {
                                        None
                                    };
                                    let album_id = alb.as_ref().and_then(|a| a.id);

                                    let mut track = tune_core::db::models::Track::new(title);
                                    track.artist_id = artist_id;
                                    track.artist_name = artist;
                                    track.album_id = album_id;
                                    track.album_title = album;
                                    track.duration_ms = duration.unwrap_or(0);
                                    track.track_number = track_num.unwrap_or(0);
                                    track.genre = genre;
                                    track.file_path = file_path;
                                    track.source = "roon_import".to_string();

                                    match track_repo.create(&track) {
                                        Ok(_) => imported += 1,
                                        Err(e) => errors.push(e),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            errors.push(format!("Roon DB query failed (schema may differ): {e}"));
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Cannot open Roon DB: {e}"));
                }
            }
        }

        let status = if errors.is_empty() { "completed" } else { "completed_with_errors" };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(task_id = tid, imported, skipped, errors = errors.len(), "roon_import_complete");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

#[derive(Deserialize)]
struct ImportPlexRequest {
    plex_url: String,
    plex_token: String,
    library_id: Option<String>,
}

async fn import_plex(
    State(state): State<AppState>,
    Json(body): Json<ImportPlexRequest>,
) -> impl IntoResponse {
    let task_id = uuid_v4();
    let db = state.db.clone();
    let plex_url = body.plex_url.trim_end_matches('/').to_string();
    let token = body.plex_token.clone();
    let library_id = body.library_id.clone();
    let tid = task_id.clone();

    let settings = SettingsRepo::new(db.clone());
    settings
        .set(
            &format!("import_task_{tid}"),
            &json!({"status": "running", "imported": 0}).to_string(),
        )
        .ok();

    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let settings = SettingsRepo::new(db.clone());
        let track_repo = TrackRepo::new(db.clone());
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());

        let mut imported = 0i32;
        let mut skipped = 0i32;
        let mut errors = Vec::<String>::new();

        // Determine which sections to import
        let section_keys: Vec<String> = if let Some(ref lid) = library_id {
            vec![lid.clone()]
        } else {
            // Fetch all library sections and filter music ones
            let sections_url = format!("{plex_url}/library/sections?X-Plex-Token={token}");
            match client
                .get(&sections_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(resp) => {
                    let data: Value = resp.json().await.unwrap_or_default();
                    data["MediaContainer"]["Directory"]
                        .as_array()
                        .map(|dirs| {
                            dirs.iter()
                                .filter(|d| d["type"].as_str() == Some("artist"))
                                .filter_map(|d| d["key"].as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                Err(e) => {
                    errors.push(format!("Failed to fetch Plex sections: {e}"));
                    vec![]
                }
            }
        };

        for sec_key in &section_keys {
            let tracks_url = format!(
                "{plex_url}/library/sections/{sec_key}/all?type=10&X-Plex-Token={token}"
            );
            let resp = match client
                .get(&tracks_url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("Section {sec_key}: {e}"));
                    continue;
                }
            };

            let data: Value = resp.json().await.unwrap_or_default();
            let tracks = match data["MediaContainer"]["Metadata"].as_array() {
                Some(t) => t,
                None => continue,
            };

            for plex_track in tracks {
                let title = plex_track["title"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if title.is_empty() {
                    continue;
                }
                let artist_name = plex_track["grandparentTitle"]
                    .as_str()
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album_title = plex_track["parentTitle"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let duration = plex_track["duration"].as_u64().unwrap_or(0) as i64;
                let track_num = plex_track["index"].as_u64().unwrap_or(0) as i32;

                // Extract file_path from Media array if available
                let file_path = plex_track["Media"]
                    .as_array()
                    .and_then(|media| media.first())
                    .and_then(|m| m["Part"].as_array())
                    .and_then(|parts| parts.first())
                    .and_then(|p| p["file"].as_str())
                    .map(|s| s.to_string());

                // Skip if we already have this track by file_path
                if let Some(ref fp) = file_path {
                    if track_repo.get_by_path(fp).ok().flatten().is_some() {
                        skipped += 1;
                        continue;
                    }
                }

                let artist = artist_repo
                    .get_or_create(&artist_name, None, None)
                    .ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = if !album_title.is_empty() {
                    album_repo
                        .get_or_create(&album_title, artist_id.unwrap_or(0), None)
                        .ok()
                } else {
                    None
                };
                let album_id = album.as_ref().and_then(|a| a.id);

                let mut new_track = tune_core::db::models::Track::new(title);
                new_track.artist_id = artist_id;
                new_track.artist_name = Some(artist_name);
                new_track.album_id = album_id;
                new_track.album_title = if album_title.is_empty() {
                    None
                } else {
                    Some(album_title)
                };
                new_track.duration_ms = duration;
                new_track.track_number = track_num;
                new_track.file_path = file_path;
                new_track.source = "plex_import".to_string();

                match track_repo.create(&new_track) {
                    Ok(_) => imported += 1,
                    Err(e) => errors.push(e),
                }
            }
        }

        let status = if errors.is_empty() {
            "completed"
        } else {
            "completed_with_errors"
        };
        settings
            .set(
                &format!("import_task_{tid}"),
                &json!({
                    "status": status,
                    "imported": imported,
                    "skipped": skipped,
                    "errors": errors.len(),
                    "error_details": errors.iter().take(20).collect::<Vec<_>>(),
                })
                .to_string(),
            )
            .ok();
        tracing::info!(task_id = tid, imported, skipped, errors = errors.len(), "plex_import_complete");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "task_id": task_id,
        })),
    )
}

async fn import_playlists_file() -> Json<Value> {
    let task_id = uuid_v4();
    Json(json!({
        "status": "accepted",
        "message": "Playlist file import not yet implemented (M3U/CSV)",
        "task_id": task_id,
    }))
}

async fn import_status(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = format!("import_task_{task_id}");
    if let Some(data) = settings.get(&key).ok().flatten() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&data) {
            return Json(json!({
                "task_id": task_id,
                "status": parsed["status"],
                "imported": parsed["imported"],
                "skipped": parsed["skipped"],
                "errors": parsed["errors"],
                "error_details": parsed["error_details"],
            }));
        }
    }
    Json(json!({
        "task_id": task_id,
        "status": "unknown",
    }))
}

/// Simple UUID v4 generator (no external crate needed).
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Pseudo-random but unique enough for task IDs
    let a = (seed & 0xFFFF_FFFF) as u32;
    let b = ((seed >> 32) & 0xFFFF) as u16;
    let c = ((seed >> 48) & 0x0FFF) as u16 | 0x4000; // version 4
    let d = ((seed >> 60) & 0x3FFF) as u16 | 0x8000; // variant
    let e = (seed.wrapping_mul(6364136223846793005) & 0xFFFF_FFFF_FFFF) as u64;
    format!("{a:08x}-{b:04x}-{c:04x}-{d:04x}-{e:012x}")
}

// ---------------------------------------------------------------------------
// Database engine: PostgreSQL test & migration
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DbConnectionTest {
    engine: String,
    connection_string: Option<String>,
}

async fn test_db_connection(Json(body): Json<DbConnectionTest>) -> impl IntoResponse {
    match body.engine.as_str() {
        "sqlite" => Json(json!({"status": "ok", "engine": "sqlite"})).into_response(),
        "postgresql" => {
            let conn_str = body
                .connection_string
                .as_deref()
                .unwrap_or("postgresql://localhost/tune");
            if conn_str.starts_with("postgresql://") || conn_str.starts_with("postgres://") {
                Json(json!({
                    "status": "ok",
                    "engine": "postgresql",
                    "message": "PostgreSQL support planned for v2.1. Connection string format is valid.",
                }))
                .into_response()
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid connection string, must start with postgresql:// or postgres://"})),
                )
                    .into_response()
            }
        }
        other => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unknown engine: {other}. Supported: sqlite, postgresql")})),
        )
            .into_response(),
    }
}

async fn migrate_database(State(state): State<AppState>) -> impl IntoResponse {
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let artists = ArtistRepo::new(state.db).count().unwrap_or(0);

    Json(json!({
        "status": "not_implemented",
        "message": "SQLite -> PostgreSQL migration planned for v2.1. Current engine: SQLite.",
        "current_engine": "sqlite",
        "row_counts": {
            "artists": artists,
            "albums": albums,
            "tracks": tracks,
        },
    }))
}

// ---------------------------------------------------------------------------
// Remote / Proxy mode
// ---------------------------------------------------------------------------

async fn get_remote_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let url = settings
        .get("remote_server_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    let enabled = settings
        .get("server_mode")
        .ok()
        .flatten()
        .map(|m| m == "remote")
        .unwrap_or(false);
    Json(json!({
        "enabled": enabled,
        "remote_url": url,
    }))
}

#[derive(Deserialize)]
struct RemoteConfig {
    remote_url: String,
}

async fn set_remote_config(
    State(state): State<AppState>,
    Json(body): Json<RemoteConfig>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("remote_server_url", &body.remote_url).ok();
    settings.set("server_mode", "remote").ok();
    Json(json!({"enabled": true, "remote_url": body.remote_url}))
}

async fn remote_status(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let url = settings
        .get("remote_server_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    if url.is_empty() {
        return Json(json!({"connected": false, "error": "no remote URL configured"}))
            .into_response();
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    match client
        .get(format!("{url}/api/v1/system/health"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(json!({"connected": true, "remote_url": url, "remote_health": data}))
                .into_response()
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            Json(json!({
                "connected": false,
                "remote_url": url,
                "error": format!("remote returned HTTP {status_code}"),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "connected": false,
            "remote_url": url,
            "error": format!("unreachable: {e}"),
        }))
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Admin: Errors / Connections / Discovery
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AdminErrorsQuery {
    lines: Option<usize>,
}

async fn admin_errors(Query(q): Query<AdminErrorsQuery>) -> Json<Value> {
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

async fn admin_connections(State(state): State<AppState>) -> Json<Value> {
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

async fn admin_discovery(State(state): State<AppState>) -> Json<Value> {
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
