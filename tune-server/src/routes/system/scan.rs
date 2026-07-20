use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// True when an `album_artist` value denotes a various-artists compilation.
fn is_various_artists(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    l == "various artists" || l == "various" || l == "va" || l == "compilations"
}

/// Decide, per `(folder, album title)`, whether that album is a various-artists
/// compilation, from the metadata of a set of scanned tracks.
///
/// A genuine single-artist album has one consistent `album_artist`. An album is
/// treated as a compilation when any of its tracks carries the compilation flag
/// or a "Various Artists" album_artist, OR when the `album_artist` value varies
/// across the tracks of the same `(folder, album)` — the tell-tale of a
/// compilation whose tracks were each tagged with their own artist as the
/// album_artist, which otherwise splits into one album (and cover) per artist.
///
/// Keys are `(folder, album_title.to_lowercase())`.
fn decide_compilation_albums<'a>(
    items: impl Iterator<Item = (String, &'a str, Option<&'a str>, bool)>,
) -> std::collections::HashMap<(String, String), bool> {
    let mut acc: std::collections::HashMap<
        (String, String),
        (bool, std::collections::HashSet<String>),
    > = std::collections::HashMap::new();
    for (dir, album, album_artist, comp_flag) in items {
        let entry = acc.entry((dir, album.to_lowercase())).or_default();
        let aa = album_artist.map(|s| s.trim()).filter(|s| !s.is_empty());
        if comp_flag || aa.map(is_various_artists).unwrap_or(false) {
            entry.0 = true;
        }
        if let Some(aa) = aa {
            entry.1.insert(aa.to_lowercase());
        }
    }
    acc.into_iter()
        .map(|(k, (flag, artists))| (k, flag || artists.len() >= 2))
        .collect()
}

#[derive(Deserialize)]
pub(super) struct ScanQuery {
    /// When true, re-process ALL discovered files (bypass the unchanged-file
    /// skip) so stale album_id assignments get re-resolved by (title, artist).
    /// Self-heals DBs corrupted by the old title-only album merge, where a
    /// track's album_id points at a wrong same-titled album. Slower (re-reads
    /// every file's metadata); default false keeps the fast incremental scan.
    force: Option<bool>,
    /// Alias for `force` sent by the clients' "Full scan / Scan complet" button.
    /// The web/Flutter clients pass `?full=true`; without this field serde
    /// silently dropped it, so "Scan complet" behaved like an ordinary
    /// incremental scan and could never re-resolve broken album/artist links —
    /// a rescan then skipped every unchanged file, so only "Vider la
    /// bibliothèque" + cold scan repaired the DB (Yacine, Synology ARM64).
    full: Option<bool>,
}

pub(super) async fn trigger_scan(
    State(state): State<AppState>,
    Query(q): Query<ScanQuery>,
) -> impl IntoResponse {
    let force = q.force.unwrap_or(false) || q.full.unwrap_or(false);
    if force {
        tracing::info!("scan_force_full_reresolve — bypassing unchanged-file skip");
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "scanning") {
        tracing::warn!(error = %e, "scan_status_set_failed");
    }
    if let Err(e) = settings.set("scan_started_at", &chrono_now()) {
        tracing::warn!(error = %e, "scan_started_at_set_failed");
    }

    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let auto_enrich_allowed = state
        .license
        .check_feature(tune_core::license::Feature::AutoEnrichment)
        .await;
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

        let list_result = tune_core::scanner::walker::list_audio_files(&music_dirs);
        let missing_dirs = list_result.missing_dirs;
        let files = list_result.files;
        let total_discovered = files.len();

        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().nfc().collect::<String>())
            .collect();

        let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(db.clone());
        let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(db.clone());
        let album_repo = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone());

        // "Separate albums by quality" — when on (default), a quality suffix is
        // appended to the album title so CD and Hi-Res versions become distinct
        // albums. The manual scan must honour it just like the file-watcher
        // (auto_scan) does, otherwise the two paths disagree (Fabien).
        let quality_split = SettingsRepo::with_backend(db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        // Load existing tracks BEFORE scanning to skip unchanged files
        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

        // Quick stat pass: skip files whose mtime+size haven't changed
        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_iter()
            .filter(|path| {
                // Force mode: re-process everything so album_id is re-resolved.
                if force {
                    return true;
                }
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

        // When a track has no album_artist tag, the album artist is pinned to
        // the first track artist seen in that folder (see below). Without this,
        // an album whose tracks have differing per-track artists (classical
        // soloists, features) split into one album row per artist (Alain,
        // Pierre: "same album appears 2-3 times"). Keyed by parent directory.
        let mut dir_album_artist: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

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
                        // A failed BEGIN means a transaction is already open on
                        // the shared connection (a previous batch that didn't
                        // commit). Roll it back and retry so the connection
                        // recovers instead of staying poisoned — which would make
                        // every playback set_queue fail for the rest of the
                        // session (Yves: stuck on the last track during a scan).
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_begin_failed");
                        let _ = db.execute_batch("ROLLBACK");
                        let _ = db.execute_batch("BEGIN IMMEDIATE");
                    }
                }

                // Decide compilation status per (folder, album title) for this
                // batch so every track of an album agrees on the album artist,
                // regardless of inconsistent per-track album_artist tags. A real
                // single-artist album has one consistent album_artist; if it
                // varies within the same (folder, album) — or any track carries
                // the compilation flag or a "Various Artists" album_artist — the
                // whole album is treated as a compilation. Without this, a
                // compilation whose tracks each carry their own artist as
                // album_artist split into one album (and cover) per artist
                // (Bilou: pochettes multipliées). Files are walked in directory
                // order so an album's tracks are contiguous and land in the same
                // batch (SCAN_BATCH_SIZE = 500).
                let comp_decision = decide_compilation_albums(batch.iter().filter_map(|sf| {
                    let meta = sf.metadata.as_ref()?;
                    let album = meta.album.as_deref()?;
                    let dir = std::path::Path::new(&sf.path)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Some((dir, album, meta.album_artist.as_deref(), meta.compilation))
                }));

                for sf in &batch {
                    let Some(ref meta) = sf.metadata else {
                        continue;
                    };

                    // Early-exit: skip unchanged files BEFORE resolving artist/album.
                    // Without this, get_or_create_with_mbid can create a ghost album
                    // entry (with cover art but no tracks) for files that are ultimately
                    // skipped — the root cause of "duplicate covers after rescan" (#593).
                    // Force mode bypasses this so album_id gets re-resolved.
                    if !force {
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
                    }

                    // Compilation status: prefer the per-(folder,album) batch
                    // decision so every track of the album agrees; fall back to
                    // this track's own signal if the album was not seen whole in
                    // this batch (rare: album straddles a batch boundary, or an
                    // incremental scan touches a single track). The fallback
                    // equals the old per-track behaviour, so incremental scans
                    // are no worse.
                    let album_dir = std::path::Path::new(&sf.path)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let is_compilation = meta
                        .album
                        .as_ref()
                        .and_then(|a| {
                            comp_decision
                                .get(&(album_dir.clone(), a.to_lowercase()))
                                .copied()
                        })
                        .unwrap_or_else(|| {
                            meta.compilation
                                || meta
                                    .album_artist
                                    .as_deref()
                                    .map(is_various_artists)
                                    .unwrap_or(false)
                        });

                    let album_artist_name = if is_compilation {
                        "Various Artists"
                    } else if let Some(aa) = meta.album_artist.as_deref() {
                        aa
                    } else {
                        // No album_artist tag: pin the album artist to the first
                        // track artist seen in this folder so all of the album's
                        // tracks resolve to a single album row, instead of
                        // splitting into one row per differing track artist
                        // (classical soloists, features).
                        let track_a = meta.artist.as_deref().unwrap_or("Unknown Artist");
                        dir_album_artist
                            .entry(album_dir.clone())
                            .or_insert_with(|| track_a.to_string())
                            .as_str()
                    };

                    let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

                    let album_artist_mbid = if is_compilation {
                        None
                    } else {
                        meta.musicbrainz_album_artist_id
                            .as_deref()
                            .or(meta.musicbrainz_artist_id.as_deref())
                    };
                    let album_artist_entry =
                        if let Some(cached) = artist_cache.get(album_artist_name) {
                            Some(std::sync::Arc::clone(cached))
                        } else {
                            let result = artist_repo
                                .get_or_create(
                                    album_artist_name,
                                    album_artist_mbid,
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

                    if let Some(ref album_title) = meta.album {
                        let t = album_title.to_lowercase();
                        if t.contains("best") || t.contains("greatest") || t.contains("hits") {
                            tracing::info!(
                                album = %album_title,
                                album_artist_tag = ?meta.album_artist,
                                artist_tag = ?meta.artist,
                                resolved_album_artist = album_artist_name,
                                resolved_artist_id = ?album_artist_id,
                                resolved_artist_name = ?album_artist_entry.as_ref().map(|a| &a.name),
                                year = ?meta.year,
                                file = %sf.path,
                                "DIAG_generic_album_scan"
                            );
                        }
                    }

                    let album_key = meta.album.as_ref().map(|t| {
                        let title = if quality_split {
                            let suffix = tune_core::scanner::quality::quality_suffix(
                                meta.sample_rate,
                                meta.bit_depth,
                            );
                            if suffix.is_empty() {
                                t.clone()
                            } else {
                                format!("{t} ({suffix})")
                            }
                        } else {
                            t.clone()
                        };
                        (title, album_artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
                    });

                    let album = if let Some(ref key) = album_key {
                        if let Some(cached) = album_cache.get(key) {
                            let c = std::sync::Arc::clone(cached);
                            if c.artist_id != Some(key.1) {
                                tracing::warn!(
                                    album = %key.0,
                                    cache_key_artist_id = key.1,
                                    cached_album_id = ?c.id,
                                    cached_album_artist_id = ?c.artist_id,
                                    file = %sf.path,
                                    "BUG_album_cache_artist_mismatch"
                                );
                            }
                            Some(c)
                        } else {
                            let result = album_repo
                                .get_or_create_with_mbid(
                                    &key.0,
                                    key.1,
                                    key.2,
                                    meta.musicbrainz_release_id.as_deref(),
                                );
                            if let Err(ref e) = result {
                                tracing::warn!(
                                    album = %key.0,
                                    artist_id = key.1,
                                    year = ?key.2,
                                    error = %e,
                                    file = %sf.path,
                                    "BUG_album_create_failed"
                                );
                            }
                            let result = result.ok().map(std::sync::Arc::new);
                            if let Some(ref a) = result {
                                if a.artist_id != Some(key.1) {
                                    tracing::warn!(
                                        album = %key.0,
                                        requested_artist_id = key.1,
                                        returned_album_id = ?a.id,
                                        returned_artist_id = ?a.artist_id,
                                        mb_release_id = ?meta.musicbrainz_release_id,
                                        file = %sf.path,
                                        "BUG_album_artist_mismatch"
                                    );
                                }
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
                            meta.year.map(|y| y as i32),
                            meta.original_year.map(|y| y as i32),
                            meta.release_date.as_deref(),
                            meta.original_date.as_deref(),
                        ).ok();
                    }

                    if let Some(aid) = album_id
                        && !albums_with_cover.contains(&aid)
                    {
                        // Prefer the embedded cover already read while parsing
                        // the tags — re-opening the file to extract it failed
                        // (os error 3, path not found) for some accented Windows
                        // paths even though the first read had succeeded
                        // (Thibaud). Fall back to a fresh extract (folder cover,
                        // or files whose metadata came from a non-tag path).
                        let cover_hash = match sf.metadata.as_ref().and_then(|m| m.cover_art.as_ref())
                        {
                            Some(cover) => tune_core::library::artwork::save_embedded_cover(
                                std::path::Path::new(&sf.path),
                                &cache_dir,
                                cover,
                            ),
                            None => tune_core::library::artwork::get_or_extract(
                                std::path::Path::new(&sf.path),
                                &cache_dir,
                            ),
                        };
                        if let Some(hash) = cover_hash {
                            if let Err(e) = album_repo.update_cover_path(aid, &hash) {
                                tracing::warn!(album_id = aid, error = %e, "cover_path_update_failed");
                            }
                            albums_with_cover.insert(aid);
                            artwork_extracted += 1;
                        }
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
                                        // Only record the image in the DB if the
                                        // cache write actually succeeded. Setting
                                        // image_path after a failed read/save left
                                        // the DB claiming "has image" with nothing
                                        // on disk → grey square + permanent skip
                                        // (Sandro, fresh install where the cache
                                        // dir wasn't writable).
                                        let saved = std::fs::read(&candidate).ok().and_then(|data| {
                                            tune_core::library::artwork::save_to_cache(
                                                &data, &cache_dir, &hash, ext,
                                            )
                                        });
                                        if saved.is_none() {
                                            tracing::warn!(
                                                artist = %art.name,
                                                candidate = %candidate.display(),
                                                "artist_image_cache_write_failed_not_recording"
                                            );
                                            continue;
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
                        // Don't leave a half-open transaction poisoning the
                        // shared connection for subsequent writes.
                        let _ = db.execute_batch("ROLLBACK");
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
                            "phase": "files",
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

        // Prune tracks whose files no longer exist on disk.
        // SAFETY: skip tracks in missing directories — the volume/NAS may
        // simply be unmounted. Deleting them would wipe the entire library.
        {
            let mut pruned = 0i64;
            let mut protected = 0i64;
            for (db_path, &(track_id, _, _)) in &existing_tracks {
                if !discovered_paths.contains(db_path.as_str()) {
                    let in_missing_dir = missing_dirs.iter().any(|d| db_path.starts_with(d));
                    if in_missing_dir {
                        protected += 1;
                        continue;
                    }
                    if track_repo.delete(track_id).is_ok() {
                        pruned += 1;
                    }
                }
            }
            if protected > 0 {
                tracing::warn!(
                    protected,
                    dirs = ?missing_dirs,
                    "post_scan_tracks_protected_missing_dirs"
                );
            }
            if pruned > 0 {
                tracing::info!(pruned, "post_scan_stale_tracks_removed");
                event_bus.emit(
                    "library.scan.progress",
                    json!({ "phase": "prune", "pruned": pruned }),
                );
            }
        }

        // Backfill + album stats in a single transaction (SQLite only)
        let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
        if !is_pg {
            if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                tracing::warn!(error = %e, "post_scan_begin_failed");
                let _ = db.execute_batch("ROLLBACK");
                let _ = db.execute_batch("BEGIN IMMEDIATE");
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

            // Full scan only: realign each album's derived genre with its tracks.
            // The COALESCE above is fill-only (it never overwrites a value once
            // set), so an album whose genre was set once and then went stale —
            // e.g. stuck on "Folk" while its tracks are now "Folk-Punk" (Yves
            // Scordia) — never self-corrected. A forced full scan is an explicit
            // "rebuild from the files" action, so overwrite genre/genres from the
            // tracks; incremental scans keep the fill-only behaviour so values
            // persist between full scans. The EXISTS guard avoids nulling an
            // album genre when no track carries one.
            if force {
                if let Err(e) = db.execute(
                    "UPDATE albums SET \
                     genre = (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL LIMIT 1), \
                     genres = (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL LIMIT 1) \
                     WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL)",
                    &[],
                ) {
                    tracing::warn!(error = %e, "post_scan_album_genre_refresh_failed");
                }
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
                     GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1",
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
                let _ = db.execute_batch("ROLLBACK");
            }
        }

        // Clean up orphan albums (album rows with no tracks). A full rescan
        // after removing files from disk — or the duplicate-album grouping —
        // can leave album rows behind that no track references. Without this
        // they linger with their cover art and inflate the total album count
        // even though they have no tracks (Alain: emptied library + full
        // rescan still shows removed albums' covers in double/triple). The
        // incremental auto-scan already purges these; the full scan did not.
        let orphan_albums = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone())
            .delete_orphans()
            .unwrap_or(0);
        if orphan_albums > 0 {
            tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
        }

        // Clean up orphan artists left behind after tag corrections
        let orphan_artists = ArtistRepo::with_backend(db.clone()).cleanup_orphans().unwrap_or(0);
        if orphan_artists > 0 {
            tracing::info!(orphan_artists, "post_scan_orphan_artists_cleaned");
        }

        // Backfill embedded cover art for local albums still missing a cover.
        // The incremental scan only extracts covers from files it re-processed;
        // unchanged files are skipped, so an improved embedded-art extractor
        // (e.g. DSF ID3v2 covers — Thibaud) never reaches an existing library.
        // Re-extract embedded art (local only, never the network) so those
        // albums self-heal without a forced full rescan.
        let covers_backfilled =
            tune_core::library::artwork::backfill_embedded_covers(&db, &cache_dir);
        if covers_backfilled > 0 {
            tracing::info!(covers_backfilled, "post_scan_embedded_covers_backfilled");
            event_bus.emit(
                "library.scan.progress",
                json!({ "phase": "artwork", "artwork_backfilled": covers_backfilled }),
            );
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

        // Populate cloud sync changelog with all new/updated entities
        tune_core::cloud::library_sync::populate_changelog_after_scan(&db);

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
                    "missing_dirs": missing_dirs.clone(),
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
                "missing_dirs": missing_dirs.clone(),
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
            "missing_dirs": missing_dirs.clone(),
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

        // Auto enrichment after scan: Premium only
        if auto_enrich_allowed {
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
        } else {
            tracing::info!("auto_enrichment_after_scan_requires_premium");
        }
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

/// GET /system/artist-split-preview — READ-ONLY dry-run of multi-artist credit
/// splitting (Phase 0 telemetry). Reports how many `artists` rows would split,
/// broken down by separator, plus example splits — WITHOUT changing anything.
/// Used to size the change and tune the allowlist before touching scan/DB.
pub(super) async fn artist_split_preview(State(state): State<AppState>) -> Json<Value> {
    use tune_core::metadata::artist_split::analyze_artist_credit;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let extra: Vec<String> = settings
        .get("artist_split_allowlist")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artists = artist_repo.list_all_id_name_mbid().unwrap_or_default();

    let total = artists.len();
    let mut would_split = 0usize;
    let mut would_split_no_mbid = 0usize;
    let mut by_sep: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    let mut examples: Vec<Value> = Vec::new();

    for (_id, name, mbid) in &artists {
        let a = analyze_artist_credit(name, &extra, true);
        if a.would_split() {
            would_split += 1;
            if mbid.is_empty() {
                would_split_no_mbid += 1;
            }
            for s in &a.separators {
                *by_sep.entry(s.as_str()).or_insert(0) += 1;
            }
            if examples.len() < 60 {
                examples.push(json!({
                    "original": a.original,
                    "tokens": a.tokens,
                    "separators": a.separators.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "has_mbid": !mbid.is_empty(),
                }));
            }
        }
    }

    Json(json!({
        "total_artists": total,
        "would_split": would_split,
        "would_split_no_mbid": would_split_no_mbid,
        "by_separator": by_sep,
        "extra_allowlist_size": extra.len(),
        "examples": examples,
        "note": "dry-run, read-only — no data changed",
    }))
}

#[cfg(test)]
mod tests {
    use super::{decide_compilation_albums, is_various_artists};

    fn decide<'a>(
        tracks: &'a [(&'a str, &'a str, Option<&'a str>, bool)],
    ) -> std::collections::HashMap<(String, String), bool> {
        decide_compilation_albums(
            tracks
                .iter()
                .map(|(dir, album, aa, flag)| (dir.to_string(), *album, *aa, *flag)),
        )
    }

    fn is_comp(
        m: &std::collections::HashMap<(String, String), bool>,
        dir: &str,
        album: &str,
    ) -> bool {
        *m.get(&(dir.to_string(), album.to_lowercase())).unwrap()
    }

    #[test]
    fn va_sentinels() {
        for s in [
            "Various Artists",
            "various",
            "VA",
            "Compilations",
            "  various artists  ",
        ] {
            assert!(is_various_artists(s), "{s} should be VA");
        }
        for s in ["The Beatles", "Various State", "AC/DC"] {
            assert!(!is_various_artists(s), "{s} should not be VA");
        }
    }

    #[test]
    fn single_artist_album_is_not_compilation() {
        // Consistent album_artist across the album -> not a compilation.
        let m = decide(&[
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
        ]);
        assert!(!is_comp(&m, "/m/beatles/abbey", "Abbey Road"));
    }

    #[test]
    fn per_track_album_artist_variance_is_compilation() {
        // The reported bug: a compilation whose tracks each carry their own
        // artist as the album_artist (no flag, no "Various Artists").
        let m = decide(&[
            ("/m/comp/jazz", "Best of Jazz", Some("Miles Davis"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("John Coltrane"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("Bill Evans"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/jazz", "Best of Jazz"));
    }

    #[test]
    fn explicit_va_album_artist_is_compilation() {
        let m = decide(&[
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/hits", "Now 100"));
    }

    #[test]
    fn compilation_flag_wins_even_with_consistent_artist() {
        let m = decide(&[
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), true),
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/ost", "OST"));
    }

    #[test]
    fn features_with_consistent_album_artist_not_compilation() {
        // Guests on some tracks, but album_artist stays the main artist -> the
        // album must not be flagged as a compilation.
        let m = decide(&[
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
        ]);
        assert!(!is_comp(&m, "/m/drake/album", "Scorpion"));
    }

    #[test]
    fn distinct_albums_same_folder_decided_independently() {
        // Two different single-artist albums sharing a folder must not be merged
        // into a compilation just because two album_artists appear in the dir.
        let m = decide(&[
            ("/m/singles", "Album A", Some("Artist A"), false),
            ("/m/singles", "Album B", Some("Artist B"), false),
        ]);
        assert!(!is_comp(&m, "/m/singles", "Album A"));
        assert!(!is_comp(&m, "/m/singles", "Album B"));
    }

    #[test]
    fn no_album_artist_is_not_flagged_compilation() {
        // Missing album_artist is left to the folder-first-artist heuristic in
        // the scan loop, not treated as a compilation here.
        let m = decide(&[
            ("/m/x/rec", "Recital", None, false),
            ("/m/x/rec", "Recital", None, false),
        ]);
        assert!(!is_comp(&m, "/m/x/rec", "Recital"));
    }

    #[test]
    fn same_album_title_different_folders_are_separate() {
        let m = decide(&[
            ("/m/a/greatest", "Greatest Hits", Some("Queen"), false),
            ("/m/b/greatest", "Greatest Hits", Some("ABBA"), false),
        ]);
        assert!(!is_comp(&m, "/m/a/greatest", "Greatest Hits"));
        assert!(!is_comp(&m, "/m/b/greatest", "Greatest Hits"));
    }
}
