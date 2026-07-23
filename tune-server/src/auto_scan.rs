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
    build_track_from_metadata_opts(sf, artist_repo, album_repo, true, None)
}

pub fn build_track_from_metadata_opts(
    sf: &ScannedFile,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
    quality_split: bool,
    // Folder-level compilation decision from the caller (the batch/watcher sees
    // an album's other tracks; a lone file can't). `None` = decide from this
    // file's own tags, the previous behaviour. Passing `Some(true)` keeps a
    // various-artists compilation whose tracks each carry their own artist as
    // album_artist from splitting into one album per artist (JP Borderies).
    compilation_override: Option<bool>,
) -> Option<(Track, Option<i64>)> {
    let meta = sf.metadata.as_ref()?;

    let is_compilation = compilation_override.unwrap_or_else(|| {
        meta.compilation
            || meta
                .album_artist
                .as_deref()
                .map(|s| s.to_lowercase())
                .map(|s| {
                    s == "various artists" || s == "various" || s == "va" || s == "compilations"
                })
                .unwrap_or(false)
    });

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
                meta.year.map(|y| y as i32),
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
        let missing_dirs = list_result.missing_dirs;
        let files = list_result.files;
        let total_discovered = files.len();
        info!(files = total_discovered, "auto_scan_files_found");

        // NFC-normalized set of every path found on disk this scan. Used after
        // the scan to prune tracks whose files were deleted while the server was
        // stopped (Symptom 2: deleted albums persist). Normalization matches how
        // existing_tracks keys are compared in the pre-filter below.
        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().nfc().collect::<String>())
            .collect();

        let track_repo = TrackRepo::with_backend(db.clone());
        let artist_repo = ArtistRepo::with_backend(db.clone());
        let album_repo = AlbumRepo::with_backend(db.clone());

        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();
        let mut known_hashes: std::collections::HashSet<(String, i64)> = track_repo
            .get_existing_audio_hash_album_pairs()
            .unwrap_or_default();

        // Keep only files that are new or whose mtime/size changed since the
        // last scan. This stat()s every discovered file; on a network mount
        // (SMB/NFS) each stat is a round-trip, so 100k files took minutes at
        // startup (Yves: "très long à démarrer"). Run the checks on a dedicated
        // thread pool oversubscribed well past the core count so the network
        // latency of many stats overlaps instead of running one at a time.
        use rayon::prelude::*;
        let is_changed = |path: &std::path::Path| -> bool {
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
        };
        let stat_pool = rayon::ThreadPoolBuilder::new().num_threads(32).build().ok();
        let files_to_scan: Vec<std::path::PathBuf> = match &stat_pool {
            Some(pool) => {
                pool.install(|| files.into_par_iter().filter(|p| is_changed(p)).collect())
            }
            None => files.into_iter().filter(|p| is_changed(p)).collect(),
        };
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

        // Progress telemetry for the auto/startup scan (parity with the manual
        // scan) so the UI shows a live bar during it too.
        let scan_total = files_to_scan.len() as i64;
        let scan_timer_start = std::time::Instant::now();
        let mut last_progress_emit = scan_timer_start;

        let stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            tune_core::scanner::walker::SCAN_BATCH_SIZE,
            |batch, batch_idx, _total_files| {
                let mut to_insert: Vec<Track> = Vec::with_capacity(batch.len());
                let mut to_update: Vec<Track> = Vec::with_capacity(batch.len() / 4);

                // Manual transaction for batch performance (SQLite only;
                // PG handles transactions at the pool level).
                if db.engine() == tune_core::db::engine::Engine::Sqlite {
                    db.execute("BEGIN IMMEDIATE", &[]).ok();
                }

                // Decide compilation status per (folder, album title) for this
                // batch — same rule as the manual scan — so every track of an
                // album agrees on the album artist even when each track was
                // tagged with its own artist as album_artist (JP Borderies:
                // compilation split into one album per artist). Falls back to
                // per-file self-decide when the album isn't wholly in this batch.
                let comp_decision = crate::routes::system::scan::decide_compilation_albums(
                    batch.iter().filter_map(|sf| {
                        let meta = sf.metadata.as_ref()?;
                        let album = meta.album.as_deref()?;
                        let dir = std::path::Path::new(&sf.path)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        Some((dir, album, meta.album_artist.as_deref(), meta.compilation))
                    }),
                );

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

                    let comp_override = sf.metadata.as_ref().and_then(|meta| {
                        let album = meta.album.as_deref()?;
                        let dir = std::path::Path::new(&sf.path)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        comp_decision.get(&(dir, album.to_lowercase())).copied()
                    });
                    let Some((mut track, album_id)) = build_track_from_metadata_opts(
                        sf,
                        &artist_repo,
                        &album_repo,
                        quality_split,
                        comp_override,
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

                // Emit scan progress after each batch (throttled every other
                // batch or 2s), mirroring the manual scan's payload/phase.
                let processed = (inserted + updated + skipped) as i64;
                if processed > 0
                    && (batch_idx % 2 == 0
                        || last_progress_emit.elapsed() >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();
                    let elapsed_secs = scan_timer_start.elapsed().as_secs_f64().max(0.001);
                    let tracks_per_second = processed as f64 / elapsed_secs;
                    let remaining = (scan_total - processed).max(0);
                    let eta_seconds = if tracks_per_second > 0.0 {
                        (remaining as f64 / tracks_per_second) as u64
                    } else {
                        0
                    };
                    event_bus.emit(
                        "library.scan.progress",
                        serde_json::json!({
                            "phase": "files",
                            "scanned": processed,
                            "added": inserted,
                            "total": scan_total,
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

        // Prune tracks whose files no longer exist on disk. The startup
        // auto-scan never removed stale rows, so files/folders deleted while
        // the server was stopped kept track_count>0 and their album was never
        // orphaned → "les albums supprimés continuent d'apparaître" (eric).
        // SAFETY: skip tracks under a missing directory (unmounted NAS / a
        // Docker mount that isn't present) — deleting them would wipe the
        // library. Mirrors the manual-scan prune (routes/system/scan.rs).
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
                    "auto_scan_tracks_protected_missing_dirs"
                );
            }
            if pruned > 0 {
                info!(pruned, "auto_scan_stale_tracks_removed");
            }
        }

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
            "missing_dirs": missing_dirs.clone(),
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
                    let had_changes = !changes.is_empty();
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

                                    // Decide compilation over the whole folder from
                                    // the siblings already in the DB, so re-importing
                                    // a single file (MP3tag save → Modified event)
                                    // doesn't split a various-artists album tagged
                                    // with per-track album_artist into one album per
                                    // artist (JP Borderies). The manual/batch scan
                                    // sees the whole album at once; the watcher sees
                                    // one file, so it reconstructs the folder view
                                    // from the DB. Any doubt → None → per-file
                                    // self-decide (previous behaviour, no regression).
                                    let comp_override: Option<bool> = sf
                                        .metadata
                                        .as_ref()
                                        .and_then(|meta| {
                                            let dir = std::path::Path::new(&sf.path).parent()?;
                                            let mut comp = meta.compilation;
                                            let mut artists: std::collections::HashSet<String> =
                                                std::collections::HashSet::new();
                                            let mut note = |aa: Option<&str>| {
                                                if let Some(a) = aa
                                                    .map(str::trim)
                                                    .filter(|s| !s.is_empty())
                                                {
                                                    if crate::routes::system::scan::is_various_artists(a) {
                                                        comp = true;
                                                    }
                                                    artists.insert(a.to_lowercase());
                                                }
                                            };
                                            note(meta.album_artist.as_deref());
                                            let siblings = track_repo
                                                .siblings_album_artists(
                                                    &dir.to_string_lossy(),
                                                )
                                                .ok()?;
                                            for (fp, aa) in &siblings {
                                                // Direct children only (exclude
                                                // sub-folders sharing the prefix).
                                                if std::path::Path::new(fp).parent()
                                                    != Some(dir)
                                                {
                                                    continue;
                                                }
                                                note(aa.as_deref());
                                            }
                                            Some(comp || artists.len() >= 2)
                                        });
                                    let Some((track, album_id)) = build_track_from_metadata_opts(
                                        sf,
                                        &artist_repo,
                                        &album_repo,
                                        watcher_quality_split,
                                        comp_override,
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
                    // After a batch, remove any album left with 0 tracks. An
                    // incremental re-import can re-point a track to a new album
                    // row (album_artist tag drift) and leave the old row as a
                    // cover-only ghost — eric: "une fois avec les pistes, une
                    // autre fois juste la pochette". The manual scan cleans
                    // these; the watcher never did.
                    if had_changes {
                        let album_repo = AlbumRepo::with_backend(db.clone());
                        let cleaned = album_repo.delete_orphans().unwrap_or(0);
                        if cleaned > 0 {
                            info!(cleaned, "watcher_orphan_albums_cleaned");
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
