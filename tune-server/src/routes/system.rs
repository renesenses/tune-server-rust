use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version", get(version))
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/config", get(get_config).patch(update_config))
        .route("/settings", get(get_settings))
        .route("/settings/theme", axum::routing::put(set_theme).get(get_theme))
        .route("/library/clear", post(library_clear))
        .route("/scan", post(trigger_scan))
        .route("/scan/status", get(scan_status))
        .route("/scan/cancel", post(scan_cancel))
        .route("/restart", post(restart))
        .route("/database/status", get(database_status))
        .route("/database/optimize", post(database_optimize))
        .route("/music-dirs", get(get_music_dirs).post(add_music_dir))
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
        .route(
            "/remote/config",
            get(get_remote_config).post(set_remote_config),
        )
        .route("/remote/status", get(remote_status))
        // Admin routes
        .route("/admin/errors", get(admin_errors))
        .route("/admin/connections", get(admin_connections))
        .route("/admin/discovery", get(admin_discovery))
        .route("/admin/health", get(admin_health))
        .route("/admin/zones", get(admin_zones))
        .route("/update/install", post(update_install))
        .route("/update/apply", post(update_apply))
        .route("/update/status", get(update_status))
        .route("/bug-report", get(generate_bug_report))
        .route("/audio-check", get(audio_check))
        .route("/enrich", post(system_enrich))
        .route("/database/import", post(database_import))
        .route("/plugins", get(list_system_plugins))
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

async fn get_settings(State(state): State<AppState>) -> Json<Value> {
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
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ThemeRequest {
    theme: String,
}

async fn set_theme(
    State(state): State<AppState>,
    Json(body): Json<ThemeRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("theme", &body.theme).ok();
    Json(json!({ "theme": body.theme }))
}

async fn get_theme(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let theme = settings.get("theme").ok().flatten();
    Json(json!({ "theme": theme }))
}

async fn library_clear(State(state): State<AppState>) -> Json<Value> {
    let repo = tune_core::db::track_repo::TrackRepo::new(state.db.clone());
    match repo.delete_all() {
        Ok(count) => {
            tracing::info!(tracks_deleted = count, "library_cleared");
            Json(json!({"ok": true, "deleted": count}))
        }
        Err(e) => {
            tracing::warn!(error = %e, "library_clear_failed");
            Json(json!({"ok": false, "error": e}))
        }
    }
}

async fn trigger_scan(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());
    settings.set("scan_status", "scanning").ok();
    settings.set("scan_started_at", &chrono_now()).ok();

    let db = state.db.clone();
    let event_bus = state.event_bus.clone();
    tokio::spawn(async move {
        let raw_dirs = get_music_dirs_list(&db);
        if raw_dirs.is_empty() {
            tracing::warn!("scan_aborted_no_dirs — no music directories configured");
            SettingsRepo::new(db).set("scan_status", "idle").ok();
            return;
        }

        // Normalize paths for cross-platform compatibility (Windows backslashes, etc.)
        let music_dirs: Vec<String> = raw_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();

        tracing::info!(
            dirs = ?music_dirs,
            platform = std::env::consts::OS,
            "scan_starting"
        );

        let files = tune_core::scanner::walker::list_audio_files(&music_dirs);
        let total_discovered = files.len();

        let track_repo = tune_core::db::track_repo::TrackRepo::new(db.clone());
        let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(db.clone());
        let album_repo = tune_core::db::album_repo::AlbumRepo::new(db.clone());

        // Load existing tracks BEFORE scanning to skip unchanged files
        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

        // Quick stat pass: skip files whose mtime+size haven't changed
        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_iter()
            .filter(|path| {
                let path_str = path.to_string_lossy();
                if let Some(&(_, existing_mtime, existing_size)) =
                    existing_tracks.get(path_str.as_ref())
                {
                    if let Ok(file_meta) = path.metadata() {
                        let mtime = file_meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let unchanged = existing_mtime
                            .map_or(false, |m| (m - mtime as f64).abs() <= 0.5)
                            && existing_size.map_or(false, |s| s == file_meta.len() as i64);
                        return !unchanged;
                    }
                }
                true
            })
            .collect();
        let pre_skipped = (total_discovered - files_to_scan.len()) as i64;

        tracing::info!(
            total = total_discovered,
            changed = files_to_scan.len(),
            unchanged = pre_skipped,
            "pre_scan_filter_complete"
        );

        event_bus.emit(
            "library.scan.started",
            json!({
                "music_dirs": &music_dirs,
                "total": total_discovered,
                "to_scan": files_to_scan.len(),
                "unchanged": pre_skipped,
            }),
        );

        // --- Batched scan + import ---
        // Parse metadata in parallel (rayon) in chunks of SCAN_BATCH_SIZE,
        // then batch-insert/update each chunk in its own transaction.
        // This gives progressive availability: tracks are queryable after
        // each batch commits, not only when the entire scan finishes.

        let cache_dir = super::library::artwork_cache_dir();
        let mut albums_with_cover: std::collections::HashSet<i64> =
            std::collections::HashSet::new();
        let mut inserted = 0i64;
        let mut updated = 0i64;
        let mut skipped = pre_skipped;
        let mut artwork_extracted = 0i64;
        let total_to_scan = files_to_scan.len() as i64;
        let total = total_to_scan + pre_skipped;
        let mut last_progress_emit = std::time::Instant::now();

        // In-memory caches to avoid repeated DB lookups (persist across batches)
        let mut artist_cache: std::collections::HashMap<String, tune_core::db::models::Artist> =
            std::collections::HashMap::new();
        let mut album_cache: std::collections::HashMap<
            (String, i64, Option<i32>),
            tune_core::db::models::Album,
        > = std::collections::HashMap::new();

        let batch_size = tune_core::scanner::walker::SCAN_BATCH_SIZE;

        // Process files in batches: parse metadata in parallel, then insert in a transaction
        let scan_stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            batch_size,
            |batch, batch_idx, _total_files| {
                // Collect tracks to batch-insert and batch-update
                let mut to_insert: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len());
                let mut to_update: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len() / 4);

                // BEGIN transaction for this batch
                db.execute_batch("BEGIN IMMEDIATE").ok();

                for sf in &batch {
                    let Some(ref meta) = sf.metadata else {
                        continue;
                    };

                    // Determine if this is a compilation (Various Artists)
                    let is_compilation = meta.compilation
                        || meta
                            .album_artist
                            .as_deref()
                            .map(|s| s.to_lowercase())
                            .map(|s| {
                                s == "various artists"
                                    || s == "various"
                                    || s == "va"
                                    || s == "compilations"
                            })
                            .unwrap_or(false);

                    // Album artist: use album_artist tag, fall back to existing album's artist
                    let existing_album_artist: Option<String> = if meta.album_artist.is_none() {
                        meta.album.as_ref().and_then(|title| {
                            album_repo
                                .get_by_title(title)
                                .ok()
                                .flatten()
                                .and_then(|a| a.artist_name)
                        })
                    } else {
                        None
                    };
                    let album_artist_name = meta
                        .album_artist
                        .as_deref()
                        .or(existing_album_artist.as_deref())
                        .unwrap_or_else(|| {
                            if is_compilation {
                                "Various Artists"
                            } else {
                                meta.artist.as_deref().unwrap_or("Unknown Artist")
                            }
                        });

                    let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

                    let album_artist_entry =
                        if let Some(cached) = artist_cache.get(album_artist_name) {
                            Some(cached.clone())
                        } else {
                            let result = artist_repo
                                .get_or_create(
                                    album_artist_name,
                                    if is_compilation {
                                        None
                                    } else {
                                        meta.musicbrainz_artist_id.as_deref()
                                    },
                                    meta.album_artist_sort.as_deref(),
                                )
                                .ok();
                            if let Some(ref a) = result {
                                artist_cache.insert(album_artist_name.to_string(), a.clone());
                            }
                            result
                        };
                    let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

                    let track_artist = if is_compilation && track_artist_name != album_artist_name {
                        if let Some(cached) = artist_cache.get(track_artist_name) {
                            Some(cached.clone())
                        } else {
                            let result = artist_repo
                                .get_or_create(
                                    track_artist_name,
                                    meta.musicbrainz_artist_id.as_deref(),
                                    None,
                                )
                                .ok();
                            if let Some(ref a) = result {
                                artist_cache.insert(track_artist_name.to_string(), a.clone());
                            }
                            result
                        }
                    } else {
                        album_artist_entry.clone()
                    };
                    let artist_id = track_artist.as_ref().and_then(|a| a.id);

                    let album_key = meta.album.as_ref().map(|t| {
                        (
                            t.clone(),
                            album_artist_id.unwrap_or(0),
                            meta.year.map(|y| y as i32),
                        )
                    });

                    let album = if let Some(ref key) = album_key {
                        if let Some(cached) = album_cache.get(key) {
                            Some(cached.clone())
                        } else {
                            let result = album_repo.get_or_create(&key.0, key.1, key.2).ok();
                            if let Some(ref a) = result {
                                album_cache.insert(key.clone(), a.clone());
                            }
                            result
                        }
                    } else {
                        None
                    };

                    let album_id = album.as_ref().and_then(|a| a.id);

                    if let Some(aid) = album_id
                        && !albums_with_cover.contains(&aid)
                        && let Some(hash) = tune_core::artwork::get_or_extract(
                            std::path::Path::new(&sf.path),
                            &cache_dir,
                        )
                    {
                        album_repo.update_cover_path(aid, &hash).ok();
                        albums_with_cover.insert(aid);
                        artwork_extracted += 1;
                    }

                    // Check for artist image if not already set
                    if let Some(ref art) = track_artist {
                        if art.image_path.is_none() {
                            if let Some(parent) = std::path::Path::new(&sf.path).parent() {
                                for name in
                                    &["artist.jpg", "artist.png", "Artist.jpg", "Artist.png"]
                                {
                                    let candidate = parent.join(name);
                                    if candidate.exists() {
                                        let hash = tune_core::artwork::artwork_hash(
                                            &candidate.to_string_lossy(),
                                        );
                                        let ext = candidate
                                            .extension()
                                            .and_then(|e| e.to_str())
                                            .unwrap_or("jpg");
                                        if let Ok(data) = std::fs::read(&candidate) {
                                            tune_core::artwork::save_to_cache(
                                                &data, &cache_dir, &hash, ext,
                                            );
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

                    let title = meta.title.clone().unwrap_or_else(|| {
                        std::path::Path::new(&sf.path)
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    });

                    // Check if this file already exists in the DB
                    if let Some(&(existing_id, existing_mtime, existing_size)) =
                        existing_tracks.get(&sf.path)
                    {
                        let file_changed = existing_mtime
                            .map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
                            || existing_size.map_or(true, |s| s != sf.file_size as i64);

                        if !file_changed {
                            skipped += 1;
                            continue;
                        }

                        // File changed -- collect for batch update
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
                        track.genres = build_genres_json(&meta.genres, meta.genre.as_deref());
                        track.composer = meta
                            .credits
                            .iter()
                            .find(|c| c.role == "composer")
                            .map(|c| c.name.clone());
                        track.year = meta.year.map(|y| y as i32);
                        track.bpm = meta.bpm;
                        track.label = meta.label.clone();
                        track.isrc = meta.isrc.clone();
                        track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
                        to_update.push(track);
                        continue;
                    }

                    // New file -- collect for batch insert
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
                    track.genres = build_genres_json(&meta.genres, meta.genre.as_deref());
                    track.composer = meta
                        .credits
                        .iter()
                        .find(|c| c.role == "composer")
                        .map(|c| c.name.clone());
                    track.year = meta.year.map(|y| y as i32);
                    track.bpm = meta.bpm;
                    track.label = meta.label.clone();
                    track.isrc = meta.isrc.clone();
                    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
                    to_insert.push(track);
                }

                // Batch insert + update using prepared statements
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as i64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as i64;
                inserted += batch_inserted;
                updated += batch_updated;

                // COMMIT this batch -- tracks are now queryable
                db.execute_batch("COMMIT").ok();

                // Emit progress after each batch
                let processed = inserted + updated + skipped;
                let elapsed = last_progress_emit.elapsed();
                if processed > 0
                    && (batch_idx % 2 == 0 || elapsed >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();
                    event_bus.emit(
                        "library.scan.progress",
                        json!({
                            "scanned": processed,
                            "total": total,
                            "batch": batch_idx,
                            "inserted": inserted,
                            "updated": updated,
                            "skipped": skipped,
                        }),
                    );
                }
            },
        );

        // Backfill + album stats in a single transaction
        db.execute_batch("BEGIN IMMEDIATE").ok();
        {
            let conn = db.connection().lock().unwrap();
            conn.execute(
                "UPDATE tracks SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                [],
            )
            .ok();
            conn.execute(
                "UPDATE albums SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                [],
            )
            .ok();
            conn.execute(
                "UPDATE albums SET track_count = \
                 (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                [],
            )
            .ok();
            conn.execute(
                "UPDATE albums SET \
                 format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                 sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                 bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                 genre = COALESCE(albums.genre, (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL LIMIT 1)), \
                 genres = COALESCE(albums.genres, (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL LIMIT 1)), \
                 disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id))",
                [],
            )
            .ok();
        }
        db.execute_batch("COMMIT").ok();

        // Clean up orphan artists left behind after tag corrections
        let orphan_artists = ArtistRepo::new(db.clone()).cleanup_orphans().unwrap_or(0);
        if orphan_artists > 0 {
            tracing::info!(orphan_artists, "post_scan_orphan_artists_cleaned");
        }

        let settings = SettingsRepo::new(db.clone());
        settings.set("scan_status", "idle").ok();
        tracing::info!(
            discovered = total_discovered,
            parsed = scan_stats.total_files,
            inserted,
            updated,
            skipped,
            artwork = artwork_extracted,
            orphan_artists,
            "scan_and_import_complete"
        );

        settings
            .set(
                "scan_result",
                &json!({
                    "total_files": total_discovered,
                    "parsed": scan_stats.total_files,
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

        event_bus.emit(
            "library.scan.completed",
            json!({
                "total_files": total_discovered,
                "parsed": scan_stats.total_files,
                "metadata_ok": scan_stats.metadata_ok,
                "inserted": inserted,
                "updated": updated,
                "skipped": skipped,
                "artwork_extracted": artwork_extracted,
            }),
        );

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
    let status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
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
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);

    if normalized.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path is empty" })),
        )
            .into_response();
    }

    let path = std::path::Path::new(&normalized);
    if !path.exists() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "directory does not exist",
                "path": normalized,
            })),
        )
            .into_response();
    }
    if !path.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "path is not a directory",
                "path": normalized,
            })),
        )
            .into_response();
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
        .set("music_dirs", &serde_json::to_string(&dirs).unwrap())
        .ok();
    Json(json!({ "dirs": dirs })).into_response()
}

async fn remove_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Json<Value> {
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
        .set("music_dirs", &serde_json::to_string(&dirs).unwrap())
        .ok();
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

/// Build a JSON array string for the `genres` column from parsed metadata.
///
/// If the structured `genres` vec is non-empty, serialize it as JSON.
/// Otherwise, fall back to the primary `genre` string and wrap it as a
/// single-element array so the column is never NULL when genre data exists.
fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre {
        if g.is_empty() {
            None
        } else {
            // Split in case genre contains separators (legacy data)
            let split = tune_core::metadata::split_genre_tag(g);
            if split.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&split).unwrap_or_default())
            }
        }
    } else {
        None
    }
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
    let artist_repo = ArtistRepo::new(state.db.clone());

    let merged_albums = merge_duplicate_albums(&state.db);
    let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
    let orphan_artists = artist_repo.cleanup_orphans().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).deduplicate().unwrap_or(0);

    let db_optimized = state.db.execute_batch("PRAGMA optimize; ANALYZE;").is_ok();

    Json(json!({
        "duplicate_albums_merged": merged_albums,
        "orphan_albums_deleted": orphan_albums,
        "orphan_artists_deleted": orphan_artists,
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
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        for &aid in &ids {
            let cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                    rusqlite::params![aid],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        for &aid in &ids {
            if aid != best_id {
                conn.execute(
                    "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    rusqlite::params![best_id, aid],
                )
                .ok();
                conn.execute("DELETE FROM albums WHERE id = ?", rusqlite::params![aid])
                    .ok();
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
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    let items = tune_core::db_backup::list_backups(&db_path);
    Json(json!(items))
}

async fn create_backup(State(state): State<AppState>) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot backup in-memory database").into_response();
    }

    state
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .ok();

    match tune_core::db_backup::create_backup(&db_path) {
        Some(info) => Json(json!(info)).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "backup failed").into_response(),
    }
}

async fn restore_backup(
    State(_state): State<AppState>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (
            StatusCode::BAD_REQUEST,
            "cannot restore to in-memory database",
        )
            .into_response();
    }

    if tune_core::db_backup::restore_backup(&db_path, &filename) {
        Json(json!({
            "restored": true,
            "filename": filename,
            "message": "restart required to apply",
        }))
        .into_response()
    } else {
        (StatusCode::NOT_FOUND, "backup not found or restore failed").into_response()
    }
}

async fn export_database(State(state): State<AppState>) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot export in-memory database").into_response();
    }

    state
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .ok();

    match tokio::fs::read(&db_path).await {
        Ok(bytes) => {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/x-sqlite3"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str("attachment; filename=\"tune_server.db\"")
                    .unwrap(),
            );
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("export failed: {e}"),
        )
            .into_response(),
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

async fn update_install(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let task_id = uuid::Uuid::new_v4().to_string();
    settings.set("update_task_id", &task_id).ok();
    settings.set("update_status", "downloading").ok();

    let tid = task_id.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap();
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;
        let url = format!(
            "https://github.com/renesenses/tune-server-rust/releases/latest/download/tune-server-{os}-{arch}"
        );
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes().await {
                    let update_path = "/tmp/tune-server-update";
                    if tokio::fs::write(update_path, &bytes).await.is_ok() {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                update_path,
                                std::fs::Permissions::from_mode(0o755),
                            )
                            .ok();
                        }
                        tracing::info!(task_id = %tid, size = bytes.len(), "update_downloaded");
                    }
                }
            }
            _ => {
                tracing::warn!(task_id = %tid, "update_download_failed");
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"task_id": task_id, "status": "downloading"})),
    )
}

async fn update_apply() -> impl IntoResponse {
    let update_path = "/tmp/tune-server-update";
    if !std::path::Path::new(update_path).exists() {
        return Json(json!({"error": "no update downloaded"})).into_response();
    }
    let current_exe = std::env::current_exe().unwrap_or_default();
    let backup = format!("{}.old", current_exe.display());
    std::fs::rename(&current_exe, &backup).ok();
    if std::fs::rename(update_path, &current_exe).is_ok() {
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        });
        Json(json!({"status": "applied", "message": "restarting with new binary"})).into_response()
    } else {
        std::fs::rename(&backup, &current_exe).ok();
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to replace binary"})),
        )
            .into_response()
    }
}

async fn update_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let status = settings
        .get("update_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let task_id = settings.get("update_task_id").ok().flatten();
    let update_exists = std::path::Path::new("/tmp/tune-server-update").exists();
    Json(json!({
        "status": status,
        "task_id": task_id,
        "update_ready": update_exists,
        "current_version": tune_core::version(),
    }))
}

async fn system_peers() -> Json<Value> {
    Json(json!([]))
}

async fn changelog() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "entries": [
            {
                "version": "0.8.6",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Play queue — file de lecture correcte en lançant album ou playlist",
                        "DLNA gapless — toggle par zone + URLs pochettes dans DIDL",
                        "Qobuz genres — format API réel pris en charge",
                        "Onboarding — onboarding_completed + genres JSON array",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "OAAT — feature flag activé pour streaming bit-perfect",
                        "Library clear — endpoint POST /system/library/clear",
                    ]},
                ]
            },
            {
                "version": "0.8.5",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Windows — scan bibliothèque retournait 0 résultats (chemins)",
                        "Spotify — lecture TUNE_SPOTIFY_CLIENT_ID pour OAuth",
                        "Squeezebox/LMS — erreur JSON-parse sur réponse vide",
                        "SSDP — énumération des vraies interfaces réseau",
                        "MP4/AAC — normalisation du format",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "Audio USB local — sortie audio via cpal",
                        "Changelog intégré dans l'interface web",
                    ]},
                ]
            },
            {
                "version": "0.8.4",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Nouveaux protocoles", "items": [
                        "AirPlay (RAOP) — lecture native sans dépendance externe",
                        "BluOS — support Bluesound (Pulse, Node, Powernode)",
                        "OpenHome — Linn et compatibles avec UPnP eventing",
                        "OAAT — découverte mDNS, transport FLAC natif, bit-perfect",
                    ]},
                    { "title": "DLNA amélioré", "items": [
                        "Retry automatique sur erreur SOAP",
                        "Détection du mute",
                        "Pochette d'album dans les métadonnées DIDL",
                        "Meilleur support DSD (format DSF explicite pour FFmpeg)",
                    ]},
                    { "title": "Nouvelles fonctionnalités", "items": [
                        "Deezer — proxy de déchiffrement intégré",
                        "DJ Player — mode DJ avec crossfade",
                        "Profils utilisateurs multi-profils",
                        "Playlist transfer entre services de streaming",
                        "Recherche full-text corrigée (FTS5)",
                        "Alarmes — scheduler avec réveil programmé",
                        "ICY metadata — titre/artiste des webradios",
                        "Enrichissement crédits MusicBrainz automatique",
                    ]},
                    { "title": "Performances et stabilité", "items": [
                        "SQLite optimisé — requêtes accélérées",
                        "Prévention des fuites mémoire (session GC, cache eviction)",
                        "SSDP optimisé (scan unique, fréquence réduite)",
                    ]},
                ]
            },
            {
                "version": "0.8.3",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Docker fix critique — binaire vide corrigé",
                        "FTS5 recherche full-text fonctionnelle",
                        "SSDP optimisé — scan unique ssdp:all",
                        "MP3 parsing relaxé",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "DMG macOS signé et notarisé (ARM + Intel)",
                        "Installer Windows setup.exe (NSIS)",
                        "Noms d'assets versionnés",
                    ]},
                ]
            },
        ]
    }))
}

async fn scan_schedule(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let time = settings
        .get("scan_schedule_time")
        .ok()
        .flatten()
        .unwrap_or_else(|| "03:00".into());
    let enabled = settings
        .get("scan_schedule_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
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
    settings
        .set(
            "scan_schedule_enabled",
            if body.enabled { "true" } else { "false" },
        )
        .ok();
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

async fn health_alerts(State(state): State<AppState>) -> Json<Value> {
    let alerts = state.health_monitor.alerts().await;
    Json(json!({ "alerts": alerts }))
}

async fn clear_cache(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("scan_result", "{}").ok();
    Json(json!({ "cleared": true }))
}

async fn get_mode(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let mode = settings
        .get("server_mode")
        .ok()
        .flatten()
        .unwrap_or_else(|| "server".into());
    Json(json!({ "mode": mode }))
}

#[derive(Deserialize)]
struct SetMode {
    mode: String,
}

async fn set_mode(State(state): State<AppState>, Json(body): Json<SetMode>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    settings.set("server_mode", &body.mode).ok();
    Json(json!({ "mode": body.mode }))
}

async fn listening_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::new(state.db);
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
                                    let (
                                        title,
                                        artist,
                                        album,
                                        file_path,
                                        duration,
                                        track_num,
                                        genre,
                                    ) = row;

                                    if let Some(ref fp) = file_path {
                                        if track_repo.get_by_path(fp).ok().flatten().is_some() {
                                            skipped += 1;
                                            continue;
                                        }
                                    }

                                    let artist_name = artist.as_deref().unwrap_or("Unknown Artist");
                                    let art =
                                        artist_repo.get_or_create(artist_name, None, None).ok();
                                    let artist_id = art.as_ref().and_then(|a| a.id);

                                    let alb = if let Some(ref album_title) = album {
                                        album_repo
                                            .get_or_create(
                                                album_title,
                                                artist_id.unwrap_or(0),
                                                None,
                                            )
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
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "roon_import_complete"
        );
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
            let tracks_url =
                format!("{plex_url}/library/sections/{sec_key}/all?type=10&X-Plex-Token={token}");
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
                let title = plex_track["title"].as_str().unwrap_or("").to_string();
                if title.is_empty() {
                    continue;
                }
                let artist_name = plex_track["grandparentTitle"]
                    .as_str()
                    .unwrap_or("Unknown Artist")
                    .to_string();
                let album_title = plex_track["parentTitle"].as_str().unwrap_or("").to_string();
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

                let artist = artist_repo.get_or_create(&artist_name, None, None).ok();
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
        tracing::info!(
            task_id = tid,
            imported,
            skipped,
            errors = errors.len(),
            "plex_import_complete"
        );
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

async fn import_status(State(state): State<AppState>, Path(task_id): Path<String>) -> Json<Value> {
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
            Json(
                json!({"error": format!("unknown engine: {other}. Supported: sqlite, postgresql")}),
            ),
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

async fn admin_health(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed().as_secs();
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let settings = SettingsRepo::new(state.db.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let zones = state.playback.all_states().await;
    let playing = zones
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
            "zones_total": zones.len(),
            "zones_playing": playing,
        },
        "outputs": output_count,
        "streaming_services": service_count,
        "scan_status": scan_status,
    }))
}

async fn admin_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
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

/// Generate a bug report with comprehensive diagnostic data.
/// Returns JSON that can also be rendered as markdown by the client.
async fn generate_bug_report(State(state): State<AppState>) -> Json<Value> {
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let uptime_secs = state.started_at.elapsed().as_secs();
    let db_version = migrations::current_version(&state.db).unwrap_or(0);
    let settings = SettingsRepo::new(state.db.clone());
    let music_dirs = get_music_dirs_list(&state.db);
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

// ---------------------------------------------------------------------------
// Audio check, Enrich, Database import, Plugins alias
// ---------------------------------------------------------------------------

async fn audio_check() -> Json<Value> {
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

async fn system_enrich(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.clone();
    let cache_dir = super::library::artwork_cache_dir();
    tokio::spawn(async move {
        tune_core::artwork::batch_enrich_artwork(db, cache_dir).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "enrichment_started" })),
    )
}

async fn database_import(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse {
    let mut file_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "database" {
            file_bytes = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(bytes) = file_bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no file provided"})),
        )
            .into_response();
    };

    // Write to a temp file
    let tmp_path = "/tmp/tune_import.db";
    if let Err(e) = std::fs::write(tmp_path, &bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write failed: {e}")})),
        )
            .into_response();
    }

    // Open the imported DB and count rows
    let import_db = match rusqlite::Connection::open_with_flags(
        tmp_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("not a valid SQLite file: {e}")})),
            )
                .into_response();
        }
    };

    let track_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
        .unwrap_or(0);
    let album_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))
        .unwrap_or(0);
    let artist_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
        .unwrap_or(0);
    drop(import_db);

    // Store the import path for potential restore
    let settings = SettingsRepo::new(state.db);
    settings.set("last_imported_db", tmp_path).ok();

    Json(json!({
        "status": "imported",
        "temp_path": tmp_path,
        "tracks": track_count,
        "albums": album_count,
        "artists": artist_count,
        "message": "Database file received. Use /system/backups to restore or merge manually.",
    }))
    .into_response()
}

async fn list_system_plugins(State(state): State<AppState>) -> Json<Value> {
    // Alias for /plugins list
    let settings = SettingsRepo::new(state.db);
    let plugins: Vec<Value> = settings
        .get("plugins")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(plugins))
}
