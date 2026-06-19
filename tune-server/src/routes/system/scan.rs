use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub(super) async fn trigger_scan(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "scanning") {
        tracing::warn!(error = %e, "scan_status_set_failed");
    }
    if let Err(e) = settings.set("scan_started_at", &chrono_now()) {
        tracing::warn!(error = %e, "scan_started_at_set_failed");
    }

    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    tokio::spawn(async move {
        let db_for_panic = db.clone();
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
        let raw_dirs = super::get_music_dirs_list(&db);
        if raw_dirs.is_empty() {
            tracing::warn!("scan_aborted_no_dirs — no music directories configured");
            if let Err(e) = SettingsRepo::with_backend(db).set("scan_status", "idle") {
                tracing::warn!(error = %e, "scan_status_reset_failed");
            }
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

        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(db.clone());
        let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(db.clone());
        let album_repo = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone());

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

        let cache_dir = crate::routes::library::artwork_cache_dir();
        let mut albums_with_cover: std::collections::HashSet<i64> =
            std::collections::HashSet::new();
        let mut inserted = 0i64;
        let mut updated = 0i64;
        let mut skipped = pre_skipped;
        let mut artwork_extracted = 0i64;
        let total_to_scan = files_to_scan.len() as i64;
        let total = total_to_scan + pre_skipped;
        let mut last_progress_emit = std::time::Instant::now();
        let scan_timer_start = std::time::Instant::now();

        // In-memory caches to avoid repeated DB lookups (persist across batches)
        let mut artist_cache: std::collections::HashMap<
            String,
            std::sync::Arc<tune_core::db::models::Artist>,
        > = std::collections::HashMap::new();
        let mut album_cache: std::collections::HashMap<
            (String, i64, Option<i32>),
            std::sync::Arc<tune_core::db::models::Album>,
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

                // BEGIN transaction for this batch (SQLite only — PG uses autocommit
                // to avoid "current transaction is aborted" cascading failures)
                let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
                if !is_pg {
                    if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_begin_failed");
                    }
                }

                for sf in &batch {
                    let Some(ref meta) = sf.metadata else {
                        continue;
                    };

                    // Early-exit: skip unchanged files BEFORE resolving artist/album.
                    // Without this, get_or_create_with_mbid can create a ghost album
                    // entry (with cover art but no tracks) for files that are ultimately
                    // skipped — the root cause of "duplicate covers after rescan" (#593).
                    if let Some(&(_existing_id, existing_mtime, existing_size)) =
                        existing_tracks.get(&sf.path)
                    {
                        let file_changed = existing_mtime
                            .map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
                            || existing_size.map_or(true, |s| s != sf.file_size as i64);
                        if !file_changed {
                            skipped += 1;
                            continue;
                        }
                    }

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
                                .get_by_title_strong(title)
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
                            Some(std::sync::Arc::clone(cached))
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
                                .ok()
                                .map(std::sync::Arc::new);
                            if let Some(ref a) = result {
                                artist_cache
                                    .insert(album_artist_name.to_string(), std::sync::Arc::clone(a));
                            }
                            result
                        };
                    let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

                    let track_artist = if is_compilation && track_artist_name != album_artist_name {
                        if let Some(cached) = artist_cache.get(track_artist_name) {
                            Some(std::sync::Arc::clone(cached))
                        } else {
                            let result = artist_repo
                                .get_or_create(
                                    track_artist_name,
                                    meta.musicbrainz_artist_id.as_deref(),
                                    None,
                                )
                                .ok()
                                .map(std::sync::Arc::new);
                            if let Some(ref a) = result {
                                artist_cache
                                    .insert(track_artist_name.to_string(), std::sync::Arc::clone(a));
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
                            Some(std::sync::Arc::clone(cached))
                        } else {
                            let result = album_repo
                                .get_or_create_with_mbid(
                                    &key.0,
                                    key.1,
                                    key.2,
                                    meta.musicbrainz_release_id.as_deref(),
                                )
                                .ok()
                                .map(std::sync::Arc::new);
                            if let Some(ref a) = result {
                                album_cache.insert(key.clone(), std::sync::Arc::clone(a));
                            }
                            result
                        }
                    } else {
                        None
                    };

                    let album_id = album.as_ref().and_then(|a| a.id);

                    // Propagate date metadata from track tags to the album
                    if let Some(aid) = album_id {
                        album_repo.update_dates(
                            aid,
                            meta.original_year.map(|y| y as i32),
                            meta.release_date.as_deref(),
                            meta.original_date.as_deref(),
                        ).ok();
                    }

                    if let Some(aid) = album_id
                        && !albums_with_cover.contains(&aid)
                        && let Some(hash) = tune_core::library::artwork::get_or_extract(
                            std::path::Path::new(&sf.path),
                            &cache_dir,
                        )
                    {
                        if let Err(e) = album_repo.update_cover_path(aid, &hash) {
                            tracing::warn!(album_id = aid, error = %e, "cover_path_update_failed");
                        }
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
                                        let hash = tune_core::library::artwork::artwork_hash(
                                            &candidate.to_string_lossy(),
                                        );
                                        let ext = candidate
                                            .extension()
                                            .and_then(|e| e.to_str())
                                            .unwrap_or("jpg");
                                        if let Ok(data) = std::fs::read(&candidate) {
                                            tune_core::library::artwork::save_to_cache(
                                                &data, &cache_dir, &hash, ext,
                                            );
                                        }
                                        let mut updated_artist =
                                            tune_core::db::models::Artist::clone(art);
                                        updated_artist.image_path = Some(hash);
                                        updated_artist.image_source = Some("local".to_string());
                                        if let Err(e) = artist_repo.update(&updated_artist) {
                                            tracing::warn!(error = %e, "artist_image_update_failed");
                                        }
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

                    // File already exists and has changed — collect for batch update
                    // (unchanged files were already skipped by the early-exit above)
                    if let Some(&(existing_id, _, _)) = existing_tracks.get(&sf.path) {
                        let mut track = tune_core::db::models::Track::new(title);
                        track.id = Some(existing_id);
                        track.album_id = album_id;
                        track.artist_id = artist_id;
                        track.artist_name = Some(track_artist_name.to_string());
                        track.album_artist = meta.album_artist.clone();
                        track.album_title = meta.album.clone();
                        track.disc_number = meta.disc_number.unwrap_or(1) as i32;
                        track.disc_subtitle = meta.disc_subtitle.clone();
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
                        track.comments = meta.comment.clone();
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
                    track.comments = meta.comment.clone();
                    to_insert.push(track);
                }

                // Collect extended metadata for tracks in this batch
                let mut extended_meta_paths: Vec<String> = Vec::new();
                for sf in &batch {
                    if sf.metadata.is_some() {
                        extended_meta_paths.push(sf.path.clone());
                    }
                }

                // Batch insert + update using prepared statements
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as i64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as i64;
                inserted += batch_inserted;
                updated += batch_updated;

                // Store extended metadata (composer, conductor, ReplayGain, MusicBrainz, etc.)
                // in the track_metadata table. Read extended tags and batch-insert.
                {
                    let meta_repo = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();

                    for path_str in &extended_meta_paths {
                        let path = std::path::Path::new(path_str);
                        // Look up the track_id by file_path
                        if let Ok(Some(track)) = track_repo.get_by_path(path_str) {
                            if let Some(track_id) = track.id {
                                let ext_meta = tune_core::metadata::read_extended_metadata(path);
                                if !ext_meta.is_empty() {
                                    meta_entries.push((track_id, ext_meta));
                                }
                            }
                        }
                    }

                    if !meta_entries.is_empty() {
                        if let Err(e) = meta_repo.set_batch_multi(&meta_entries) {
                            tracing::warn!(error = %e, "scan_extended_metadata_insert_failed");
                        }
                    }
                }

                // Update track_count + album stats for albums touched in this batch
                // so albums are never visible with 0 tracks between batches.
                {
                    let touched_album_ids: std::collections::HashSet<i64> = to_insert
                        .iter()
                        .chain(to_update.iter())
                        .filter_map(|t| t.album_id)
                        .collect();
                    if !touched_album_ids.is_empty() {
                        let ids_csv: String = touched_album_ids
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        db.execute_batch(&format!(
                            "UPDATE albums SET track_count = \
                             (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id) \
                             WHERE id IN ({ids_csv});\
                             UPDATE albums SET \
                             format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                             sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                             bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                             genre = COALESCE(albums.genre, (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL LIMIT 1)), \
                             disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id)) \
                             WHERE id IN ({ids_csv})"
                        )).ok();
                    }
                }

                // COMMIT this batch -- tracks + album stats are now queryable
                if !is_pg {
                    if let Err(e) = db.execute_batch("COMMIT") {
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_commit_failed");
                    }
                }

                // Emit progress after each batch
                let processed = inserted + updated + skipped;
                let elapsed = last_progress_emit.elapsed();
                if processed > 0
                    && (batch_idx % 2 == 0 || elapsed >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();

                    // Compute scan rate and ETA
                    let elapsed_secs = scan_timer_start.elapsed().as_secs_f64().max(0.001);
                    let tracks_per_second = processed as f64 / elapsed_secs;
                    let remaining = (total - processed).max(0);
                    let eta_seconds = if tracks_per_second > 0.0 {
                        (remaining as f64 / tracks_per_second) as u64
                    } else {
                        0
                    };

                    event_bus.emit(
                        "library.scan.progress",
                        json!({
                            "scanned": processed,
                            "added": inserted,
                            "total": total,
                            "batch": batch_idx,
                            "inserted": inserted,
                            "updated": updated,
                            "skipped": skipped,
                            "tracks_per_second": (tracks_per_second * 10.0).round() / 10.0,
                            "eta_seconds": eta_seconds,
                        }),
                    );
                }
            },
        );

        // Prune tracks whose files no longer exist on disk
        {
            let mut pruned = 0i64;
            for (db_path, &(track_id, _, _)) in &existing_tracks {
                if !discovered_paths.contains(db_path.as_str()) {
                    if track_repo.delete(track_id).is_ok() {
                        pruned += 1;
                    }
                }
            }
            if pruned > 0 {
                tracing::info!(pruned, "post_scan_stale_tracks_removed");
                event_bus.emit(
                    "library.scan.progress",
                    json!({ "pruned": pruned }),
                );
            }
        }

        // Backfill + album stats in a single transaction (SQLite only)
        let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
        if !is_pg {
            if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                tracing::warn!(error = %e, "post_scan_begin_failed");
            }
        }
        {
            if let Err(e) = db.execute(
                "UPDATE tracks SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET track_count = \
                 (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_count_update_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET \
                 format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                 sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                 bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                 genre = COALESCE(albums.genre, (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL LIMIT 1)), \
                 genres = COALESCE(albums.genres, (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL LIMIT 1)), \
                 disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id))",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_quality_update_failed");
            }
            // Merge duplicate local albums (same title, case-insensitive).
            // After a rescan, tag changes can create a second album entry for
            // tracks that already belonged to an existing album (e.g. when
            // album_artist changed). Merging moves all tracks to the album
            // with the most tracks, so the orphan cleanup below can delete the
            // now-empty duplicate. This is the definitive fix for bug #593
            // ("Doublons pochettes albums apres rescan").
            {
                let dupe_rows = db.query_many(
                    "SELECT LOWER(title), GROUP_CONCAT(id) FROM albums \
                     WHERE source = 'local' \
                     GROUP BY LOWER(title) HAVING COUNT(id) > 1",
                    &[],
                ).unwrap_or_default();
                let dupes: Vec<(String, String)> = dupe_rows.iter().map(|r| {
                    (r[0].as_string().unwrap_or_default(), r[1].as_string().unwrap_or_default())
                }).collect();
                let mut merged_albums = 0usize;
                for (_title, ids_str) in &dupes {
                    let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
                    if ids.len() < 2 {
                        continue;
                    }
                    // Keep the album with the most tracks
                    let mut best_id = ids[0];
                    let mut best_count = 0i64;
                    for &aid in &ids {
                        let cnt = db.query_one(
                            "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                            &[&aid],
                        ).ok().flatten().and_then(|r| r[0].as_i64()).unwrap_or(0);
                        if cnt > best_count {
                            best_count = cnt;
                            best_id = aid;
                        }
                    }
                    for &aid in &ids {
                        if aid != best_id {
                            db.execute(
                                "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                                &[&best_id, &aid],
                            ).ok();
                            db.execute(
                                "DELETE FROM albums WHERE id = ?",
                                &[&aid],
                            ).ok();
                            merged_albums += 1;
                        }
                    }
                }
                if merged_albums > 0 {
                    // Refresh track_count for albums that received tracks from merged duplicates
                    db.execute_batch(
                        "UPDATE albums SET track_count = \
                         (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                    ).ok();
                    tracing::info!(merged_albums, "post_scan_duplicate_albums_merged");
                }
            }
            // Remove orphan albums with 0 tracks (created by interrupted scans or tag changes)
            let orphan_albums = db.execute(
                "DELETE FROM albums WHERE id IN (\
                 SELECT a.id FROM albums a \
                 LEFT JOIN tracks t ON t.album_id = a.id \
                 WHERE t.id IS NULL AND a.source = 'local')",
                &[],
            ).unwrap_or(0);
            if orphan_albums > 0 {
                tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
            }
        }
        if !is_pg {
            if let Err(e) = db.execute_batch("COMMIT") {
                tracing::warn!(error = %e, "post_scan_commit_failed");
            }
        }

        // Clean up orphan artists left behind after tag corrections
        let orphan_artists = ArtistRepo::with_backend(db.clone()).cleanup_orphans().unwrap_or(0);
        if orphan_artists > 0 {
            tracing::info!(orphan_artists, "post_scan_orphan_artists_cleaned");
        }

        // Rebuild FTS indexes so search reflects the current library state.
        // The FTS tables are contentless (content='') and rely on triggers,
        // but manual DB edits or batch operations can leave them stale.
        // A full rebuild after scan guarantees consistency.
        // FTS rebuild + WAL checkpoint are SQLite-specific operations
        if db.engine() == tune_core::db::engine::Engine::Sqlite {
            db.execute_batch(
                "INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all');\
                 INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
                 SELECT t.id, t.title, ar.name, al.title, t.genre, t.composer \
                 FROM tracks t LEFT JOIN artists ar ON t.artist_id = ar.id LEFT JOIN albums al ON t.album_id = al.id;\
                 INSERT INTO albums_fts(albums_fts) VALUES('delete-all');\
                 INSERT INTO albums_fts(rowid, title, artist_name, genre) \
                 SELECT a.id, a.title, ar.name, a.genre FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id;\
                 INSERT INTO artists_fts(artists_fts) VALUES('delete-all');\
                 INSERT INTO artists_fts(rowid, name, sort_name) SELECT id, name, sort_name FROM artists;\
                 PRAGMA wal_checkpoint(PASSIVE);",
            ).ok();
            tracing::info!("post_scan_fts_rebuilt");
        }

        let settings = SettingsRepo::with_backend(db.clone());
        if let Err(e) = settings.set("scan_status", "idle") {
            tracing::warn!(error = %e, "scan_status_idle_failed");
        }
        tracing::info!(
            discovered = total_discovered,
            parsed = scan_stats.total_files,
            timeout = scan_stats.metadata_timeout,
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
                    "metadata_timeout": scan_stats.metadata_timeout,
                    "inserted": inserted,
                    "updated": updated,
                    "skipped": skipped,
                    "artwork_extracted": artwork_extracted,
                    "failed_paths": scan_stats.failed_paths,
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
                "metadata_timeout": scan_stats.metadata_timeout,
                "inserted": inserted,
                "updated": updated,
                "skipped": skipped,
                "artwork_extracted": artwork_extracted,
                "failed_paths": scan_stats.failed_paths,
            }),
        );

        // Launch batch artwork enrichment as a background task
        // This fetches covers from MusicBrainz Cover Art Archive for albums
        // that don't have embedded cover art.
        // Write scan report JSON for the /scan/report endpoint
        let report = serde_json::json!({
            "total_files": total_discovered,
            "parsed": scan_stats.total_files,
            "metadata_ok": scan_stats.metadata_ok,
            "metadata_failed": scan_stats.metadata_failed,
            "metadata_timeout": scan_stats.metadata_timeout,
            "inserted": inserted,
            "updated": updated,
            "skipped": skipped,
            "artwork_extracted": artwork_extracted,
            "failed_paths": scan_stats.failed_paths,
        });
        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(&report_path, json).ok();
        }

        let enrich_db = db.clone();
        let artist_cache_dir = cache_dir.clone();
        let artist_enrich_db = db.clone();
        handle.spawn(async move {
            tune_core::library::artwork::batch_enrich_artwork(enrich_db, cache_dir).await;
        });

        handle.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            tune_core::library::artwork::batch_enrich_artist_artwork(artist_enrich_db, artist_cache_dir).await;
        });
        }).await;
        if let Err(e) = result {
            tracing::error!("scan_task_panicked — {:?}", e);
            if let Err(e2) = SettingsRepo::with_backend(db_for_panic).set("scan_status", "idle") {
                tracing::warn!(error = %e2, "scan_status_panic_reset_failed");
            }
        }
    });

    (StatusCode::ACCEPTED, Json(json!({ "status": "scanning" })))
}

pub(super) async fn scan_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

pub(super) async fn scan_cancel(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "idle") {
        tracing::warn!(error = %e, "scan_cancel_status_reset_failed");
    }
    StatusCode::NO_CONTENT
}

pub(super) async fn scan_schedule(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
pub(super) struct ScanScheduleReq {
    enabled: bool,
    time: Option<String>,
}

pub(super) async fn set_scan_schedule(
    State(state): State<AppState>,
    Json(body): Json<ScanScheduleReq>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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

pub(super) async fn library_clear(State(state): State<AppState>) -> Json<Value> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    match repo.delete_all() {
        Ok(count) => {
            tracing::info!(tracks_deleted = count, "library_cleared");
            Json(json!({"ok": true, "deleted": count}))
        }
        Err(e) => {
            tracing::warn!(error = %e, "library_clear_failed");
            Json(json!({"ok": false, "error": e.to_string()}))
        }
    }
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
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

pub(super) async fn scan_report() -> impl IntoResponse {
    let report_path = std::env::var("TUNE_DB_PATH")
        .unwrap_or_else(|_| "tune.db".into())
        .replace(".db", "-scan-report.json");
    match std::fs::read_to_string(&report_path) {
        Ok(json) => match serde_json::from_str::<Value>(&json) {
            Ok(v) => Json(v).into_response(),
            Err(_) => Json(json!({"error": "invalid report file"})).into_response(),
        },
        Err(_) => Json(json!({"error": "no scan report available yet"})).into_response(),
    }
}
