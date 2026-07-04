use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;
use unicode_normalization::UnicodeNormalization;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::EventBus;
use tune_core::scanner::walker::ScannedFile;

/// Build a `Track` from scanned file metadata, resolving artist/album in the DB.
///
/// Returns `(track, album_id, is_compilation)` or `None` if metadata is missing.
pub fn build_track_from_metadata(
    sf: &ScannedFile,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
) -> Option<(Track, Option<i64>)> {
    build_track_from_metadata_opts(sf, artist_repo, album_repo, true)
}

pub fn build_track_from_metadata_opts(
    sf: &ScannedFile,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
    quality_split: bool,
) -> Option<(Track, Option<i64>)> {
    let meta = sf.metadata.as_ref()?;

    let is_compilation = meta.compilation
        || meta
            .album_artist
            .as_deref()
            .map(|s| s.to_lowercase())
            .map(|s| s == "various artists" || s == "various" || s == "va" || s == "compilations")
            .unwrap_or(false);

    let album_artist_name = if is_compilation {
        "Various Artists"
    } else {
        meta.album_artist
            .as_deref()
            .unwrap_or_else(|| meta.artist.as_deref().unwrap_or("Unknown Artist"))
    };

    let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

    let album_artist_mbid = if is_compilation {
        None
    } else {
        meta.musicbrainz_album_artist_id
            .as_deref()
            .or(meta.musicbrainz_artist_id.as_deref())
    };
    let album_artist_entry = match artist_repo.get_or_create(
        album_artist_name,
        album_artist_mbid,
        meta.album_artist_sort.as_deref(),
    ) {
        Ok(a) => {
            if let Some(ref mbid) = a.musicbrainz_id {
                if a.name.to_lowercase() != album_artist_name.to_lowercase() {
                    tracing::warn!(
                        expected = album_artist_name,
                        resolved = %a.name,
                        mbid = %mbid,
                        file = %sf.path,
                        "album_artist_mbid_name_mismatch"
                    );
                }
            }
            Some(a)
        }
        Err(e) => {
            tracing::warn!(
                artist = album_artist_name,
                error = %e,
                file = %sf.path,
                "album_artist_create_failed_skipping_track"
            );
            return None;
        }
    };
    let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

    let track_artist = if is_compilation && track_artist_name != album_artist_name {
        match artist_repo.get_or_create(
            track_artist_name,
            meta.musicbrainz_artist_id.as_deref(),
            None,
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(artist = track_artist_name, error = %e, "track_artist_create_failed");
                album_artist_entry.clone()
            }
        }
    } else {
        album_artist_entry.clone()
    };
    let artist_id = track_artist.as_ref().and_then(|a| a.id);

    let album = meta.album.as_ref().and_then(|title| {
        let Some(aid) = album_artist_id else {
            tracing::warn!(album = title, file = %sf.path, "album_skipped_no_artist_id");
            return None;
        };
        tracing::debug!(
            album = %title,
            album_artist_tag = ?meta.album_artist,
            album_artist_resolved = album_artist_name,
            album_artist_id = aid,
            album_artist_mbid = ?album_artist_mbid,
            track_artist = track_artist_name,
            mb_artist_id = ?meta.musicbrainz_artist_id,
            mb_album_artist_id = ?meta.musicbrainz_album_artist_id,
            file = %sf.path,
            "DIAG_album_resolution"
        );
        // Quality-based album splitting: append suffix when sample_rate or
        // bit_depth indicate a different quality tier (e.g. "Album (96kHz/24bit)").
        // This prevents WAV 96kHz, WAV 44kHz, and MP3 from being merged.
        let suffix = if quality_split {
            tune_core::scanner::quality::quality_suffix(meta.sample_rate, meta.bit_depth)
        } else {
            String::new()
        };
        let album_title = if suffix.is_empty() {
            title.clone()
        } else {
            format!("{title} ({suffix})")
        };
        // get_or_create uses (title, artist_id, year) to disambiguate albums.
        // Compilations use the "Various Artists" artist_id, so same-title albums
        // by different artists are correctly kept separate.
        album_repo
            .get_or_create(&album_title, aid, meta.year.map(|y| y as i32))
            .ok()
    });
    let album_id = album.as_ref().and_then(|a| a.id);

    // Propagate date metadata from track tags to the album (COALESCE — only
    // fills in values not already set, so the first track with dates wins).
    if let Some(aid) = album_id {
        album_repo
            .update_dates(
                aid,
                meta.original_year.map(|y| y as i32),
                meta.release_date.as_deref(),
                meta.original_date.as_deref(),
            )
            .ok();
    }

    let title = meta.title.clone().unwrap_or_else(|| {
        std::path::Path::new(&sf.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let mut track = Track::new(title);
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
    track.year = meta.year.map(|y| y as i32);
    track.label = meta.label.clone();
    track.isrc = meta.isrc.clone();
    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
    track.comments = meta.comment.clone();

    Some((track, album_id))
}

/// Spawn the auto-scan task that indexes all music directories at startup.
///
/// Returns an `Arc<AtomicBool>` that is set to `true` once the scan finishes.
/// The file watcher should wait for this flag before monitoring directories,
/// otherwise it may pick up filesystem events triggered by the scan itself
/// (macOS FSEvents can replay recent events on watcher startup) and race
/// with the scanner — deleting freshly inserted tracks.
pub fn spawn_auto_scan(db: Arc<dyn DbBackend>, event_bus: Arc<EventBus>) -> Arc<AtomicBool> {
    let scan_done = Arc::new(AtomicBool::new(false));
    let scan_done_clone = scan_done.clone();
    tokio::task::spawn_blocking(move || {
        info!("auto_scan_starting");
        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
        let raw_dirs: Vec<String> = settings
            .get("music_dirs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let music_dirs: Vec<String> = raw_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();

        if music_dirs.is_empty() {
            info!("auto_scan_skipped_no_dirs");
            return;
        }

        let list_result = tune_core::scanner::walker::list_audio_files(&music_dirs);
        let files = list_result.files;
        let total_discovered = files.len();
        info!(files = total_discovered, "auto_scan_files_found");

        let track_repo = TrackRepo::with_backend(db.clone());
        let artist_repo = ArtistRepo::with_backend(db.clone());
        let album_repo = AlbumRepo::with_backend(db.clone());

        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();
        let mut known_hashes: std::collections::HashSet<(String, i64)> = track_repo
            .get_existing_audio_hash_album_pairs()
            .unwrap_or_default();

        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_iter()
            .filter(|path| {
                let path_str: String = path.to_string_lossy().nfc().collect();
                if let Some(&(_, existing_mtime, existing_size)) =
                    existing_tracks.get(path_str.as_str())
                    && let Ok(file_meta) = path.metadata()
                {
                    let mtime = file_meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let unchanged = existing_mtime.is_some_and(|m| (m - mtime as f64).abs() <= 0.5)
                        && (existing_size == Some(file_meta.len() as i64));
                    return !unchanged;
                }
                true
            })
            .collect();
        let pre_skipped = total_discovered - files_to_scan.len();

        info!(
            total = total_discovered,
            changed = files_to_scan.len(),
            unchanged = pre_skipped,
            "auto_scan_pre_filter_complete"
        );

        event_bus.emit(
            "library.scan.started",
            serde_json::json!({
                "music_dirs": &music_dirs,
                "total": total_discovered,
                "to_scan": files_to_scan.len(),
                "unchanged": pre_skipped,
                "auto": true,
            }),
        );

        let cache_dir = crate::routes::library::artwork_cache_dir();
        info!(cache_dir = %cache_dir.display(), "artwork_cache_dir_resolved");
        let quality_split = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let mut albums_with_cover: std::collections::HashSet<i64> =
            std::collections::HashSet::new();
        let mut inserted = 0u64;
        let mut updated = 0u64;
        let mut skipped = pre_skipped as u64;

        let stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            tune_core::scanner::walker::SCAN_BATCH_SIZE,
            |batch, _batch_idx, _total_files| {
                let mut to_insert: Vec<Track> = Vec::with_capacity(batch.len());
                let mut to_update: Vec<Track> = Vec::with_capacity(batch.len() / 4);

                // Manual transaction for batch performance (SQLite only;
                // PG handles transactions at the pool level).
                if db.engine() == tune_core::db::engine::Engine::Sqlite {
                    db.execute("BEGIN IMMEDIATE", &[]).ok();
                }

                for sf in &batch {
                    if sf.metadata.is_none() {
                        tracing::warn!(path = %sf.path, "scan_track_skipped_no_metadata");
                        continue;
                    }

                    // Early-exit: skip unchanged files BEFORE resolving artist/album.
                    // Without this, build_track_from_metadata can create a ghost album
                    // (with cover but no tracks) for files that are ultimately skipped.
                    if let Some(&(_existing_id, existing_mtime, existing_size)) =
                        existing_tracks.get(&sf.path)
                    {
                        let file_changed = existing_mtime
                            .is_none_or(|m| (m - sf.mtime as f64).abs() > 0.5)
                            || (existing_size != Some(sf.file_size as i64));
                        if !file_changed {
                            skipped += 1;
                            continue;
                        }
                    }

                    let Some((mut track, album_id)) = build_track_from_metadata_opts(
                        sf,
                        &artist_repo,
                        &album_repo,
                        quality_split,
                    ) else {
                        continue;
                    };

                    if let Some(aid) = album_id
                        && !albums_with_cover.contains(&aid)
                        && let Some(hash) = tune_core::library::artwork::get_or_extract(
                            std::path::Path::new(&sf.path),
                            &cache_dir,
                        )
                    {
                        album_repo.update_cover_path(aid, &hash).ok();
                        albums_with_cover.insert(aid);
                    }

                    // File already exists and has changed — collect for batch update
                    if let Some(&(existing_id, _, _)) = existing_tracks.get(&sf.path) {
                        track.id = Some(existing_id);
                        to_update.push(track);
                        continue;
                    }

                    // Deduplicate by audio_hash + album_id: if the same content
                    // already exists in this album (via a different path), skip it.
                    if let (Some(hash), Some(aid)) = (&track.audio_hash, track.album_id) {
                        let key = (hash.clone(), aid);
                        if known_hashes.contains(&key) {
                            tracing::debug!(
                                audio_hash = %hash,
                                album_id = aid,
                                path = %sf.path,
                                "skip_duplicate_audio_hash"
                            );
                            skipped += 1;
                            continue;
                        }
                        known_hashes.insert(key);
                    }

                    to_insert.push(track);
                }

                inserted += track_repo.create_batch(&to_insert).unwrap_or(0) as u64;
                updated += track_repo.update_batch(&to_update).unwrap_or(0) as u64;

                // Extract extended metadata (ISRC, ReplayGain, MusicBrainz, lyrics, etc.)
                {
                    let meta_repo =
                        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(
                            db.clone(),
                        );
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> =
                        Vec::new();
                    for sf in &batch {
                        if sf.metadata.is_some() {
                            let path = std::path::Path::new(&sf.path);
                            if let Ok(Some(track)) = track_repo.get_by_path(&sf.path) {
                                if let Some(track_id) = track.id {
                                    let ext = tune_core::metadata::read_extended_metadata(path);
                                    if !ext.is_empty() {
                                        meta_entries.push((track_id, ext));
                                    }
                                }
                            }
                        }
                    }
                    if !meta_entries.is_empty() {
                        meta_repo.set_batch_multi(&meta_entries).ok();
                    }
                }

                if db.engine() == tune_core::db::engine::Engine::Sqlite {
                    db.execute("COMMIT", &[]).ok();
                }
            },
        );

        for album in album_repo.list(99999, 0).unwrap_or_default() {
            if let Some(id) = album.id {
                album_repo.update_track_count(id).ok();
                album_repo.update_quality_from_tracks(id).ok();
            }
        }

        // Clean up orphan albums with 0 tracks (ghost entries from
        // artist_id changes or interrupted scans) — bug #593.
        let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
        if orphan_albums > 0 {
            info!(orphan_albums, "auto_scan_orphan_albums_cleaned");
        }

        info!(
            total = stats.total_files,
            ok = stats.metadata_ok,
            failed = stats.metadata_failed,
            timeout = stats.metadata_timeout,
            inserted,
            updated,
            skipped,
            artwork = albums_with_cover.len(),
            orphan_albums,
            "auto_scan_complete"
        );

        let report = serde_json::json!({
            "total_files": stats.total_files,
            "metadata_ok": stats.metadata_ok,
            "metadata_failed": stats.metadata_failed,
            "metadata_timeout": stats.metadata_timeout,
            "inserted": inserted,
            "updated": updated,
            "skipped": skipped,
            "artwork_extracted": albums_with_cover.len(),
            "failed_paths": stats.failed_paths,
        });

        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(&report_path, json).ok();
        }

        event_bus.emit("library.scan.completed", report);
        scan_done_clone.store(true, Ordering::Release);
    });
    scan_done
}

/// Spawn the file watcher that monitors music directories for live changes.
///
/// If `wait_for_scan` is provided, the watcher will wait until the initial scan
/// completes before starting to monitor directories. This prevents the watcher
/// from picking up stale FSEvents replayed on subscription and racing with the
/// scanner (deleting tracks that the scanner just inserted).
pub fn spawn_file_watcher(db: Arc<dyn DbBackend>, wait_for_scan: Option<Arc<AtomicBool>>) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if music_dirs.is_empty() {
        return;
    }

    match tune_core::scanner::watcher::FileWatcher::new(music_dirs) {
        Ok(watcher) => {
            info!("file_watcher_started");
            tokio::task::spawn_blocking(move || {
                // Wait for the initial auto-scan to complete before processing
                // watcher events. On macOS, FSEvents replays recent events when
                // a new watcher subscribes, which can cause the watcher to
                // delete+reinsert tracks that the scanner just added.
                if let Some(ref flag) = wait_for_scan {
                    info!("file_watcher_waiting_for_scan");
                    while !flag.load(Ordering::Acquire) {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    info!("file_watcher_scan_complete_starting_watch");
                }
                // Always drain stale events before entering the watch loop.
                // On macOS, FSEvents replays recent events from the persistent
                // journal when a new stream is created, even with
                // kFSEventStreamEventIdSinceNow.  Give it 2 seconds to flush
                // (the default FSEvents coalescing latency) to avoid
                // reprocessing events that already happened before startup.
                std::thread::sleep(std::time::Duration::from_secs(2));
                let stale = watcher.poll_changes(std::time::Duration::from_millis(200));
                if !stale.is_empty() {
                    info!(count = stale.len(), "file_watcher_drained_stale_events");
                }
                let watcher_quality_split =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
                        .get("quality_split")
                        .ok()
                        .flatten()
                        .map(|v| v != "false" && v != "0")
                        .unwrap_or(true);
                loop {
                    let changes = watcher.poll_debounced(
                        std::time::Duration::from_secs(2),
                        std::time::Duration::from_millis(500),
                    );
                    for change in changes {
                        match change.change_type {
                            tune_core::scanner::watcher::ChangeType::Added
                            | tune_core::scanner::watcher::ChangeType::Modified => {
                                // Unchanged-file guard (Jean Marie: "le scan tourne
                                // en boucle", macOS Ventura). A Modified event whose
                                // on-disk mtime+size still match the stored row is a
                                // self-induced event: reading a file to import it
                                // makes macOS write an extended attribute, which
                                // fires another Modify event → re-read → infinite
                                // loop. Detect it with a cheap stat and skip —
                                // crucially WITHOUT reading the content (scan_files_
                                // parallel), since the read is what re-triggers it.
                                if change.change_type
                                    == tune_core::scanner::watcher::ChangeType::Modified
                                {
                                    if let Ok(Some(existing)) = TrackRepo::with_backend(db.clone())
                                        .get_by_path(&change.path)
                                    {
                                        if let Ok(fs_meta) = std::fs::metadata(&change.path) {
                                            let fs_size = fs_meta.len() as i64;
                                            let fs_mtime = fs_meta
                                                .modified()
                                                .ok()
                                                .and_then(|t| {
                                                    t.duration_since(std::time::UNIX_EPOCH).ok()
                                                })
                                                .map(|d| d.as_secs() as f64);
                                            let unchanged =
                                                existing.file_size.map_or(false, |s| s == fs_size)
                                                    && match (existing.file_mtime, fs_mtime) {
                                                        (Some(a), Some(b)) => (a - b).abs() <= 0.5,
                                                        _ => false,
                                                    };
                                            if unchanged {
                                                tracing::debug!(path = %change.path, "watcher_skip_unchanged");
                                                continue;
                                            }
                                        }
                                    }
                                }
                                let files: Vec<std::path::PathBuf> =
                                    vec![std::path::PathBuf::from(&change.path)];
                                let (scanned, _) = tune_core::scanner::walker::scan_files_parallel(
                                    &files, true, None,
                                );
                                let track_repo = TrackRepo::with_backend(db.clone());
                                let artist_repo = ArtistRepo::with_backend(db.clone());
                                let album_repo = AlbumRepo::with_backend(db.clone());

                                for sf in &scanned {
                                    if sf.metadata.is_none() {
                                        continue;
                                    }

                                    if change.change_type
                                        == tune_core::scanner::watcher::ChangeType::Modified
                                    {
                                        track_repo.delete_by_path(&sf.path).ok();
                                    }

                                    let Some((track, album_id)) = build_track_from_metadata_opts(
                                        sf,
                                        &artist_repo,
                                        &album_repo,
                                        watcher_quality_split,
                                    ) else {
                                        tracing::warn!(path = %sf.path, "watcher_track_skipped_no_metadata");
                                        continue;
                                    };

                                    // Skip duplicate: same audio content already in this album
                                    if let (Some(hash), Some(aid)) = (&track.audio_hash, album_id) {
                                        if track_repo
                                            .exists_by_audio_hash_and_album(hash, aid)
                                            .unwrap_or(false)
                                        {
                                            tracing::debug!(
                                                audio_hash = %hash,
                                                album_id = aid,
                                                path = %sf.path,
                                                "watcher_skip_duplicate_audio_hash"
                                            );
                                            continue;
                                        }
                                    }

                                    if let Some(aid) = album_id {
                                        let cache_dir = crate::routes::library::artwork_cache_dir();
                                        if let Some(hash) =
                                            tune_core::library::artwork::get_or_extract(
                                                std::path::Path::new(&sf.path),
                                                &cache_dir,
                                            )
                                        {
                                            album_repo.update_cover_path(aid, &hash).ok();
                                        }
                                        album_repo.update_track_count(aid).ok();
                                        album_repo.update_quality_from_tracks(aid).ok();
                                    }

                                    if track_repo.create(&track).is_ok() {
                                        info!(path = %sf.path, "watcher_track_added");
                                    }
                                }
                            }
                            tune_core::scanner::watcher::ChangeType::Deleted => {
                                let track_repo = TrackRepo::with_backend(db.clone());
                                if track_repo.delete_by_path(&change.path).is_ok() {
                                    info!(path = %change.path, "watcher_track_removed");
                                }
                            }
                        }
                    }
                }
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "file_watcher_init_failed");
        }
    }
}
