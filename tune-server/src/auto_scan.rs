use std::sync::Arc;

use tracing::info;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
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
    let meta = sf.metadata.as_ref()?;

    let is_compilation = meta.compilation
        || meta
            .album_artist
            .as_deref()
            .map(|s| s.to_lowercase())
            .map(|s| {
                s == "various artists" || s == "various" || s == "va" || s == "compilations"
            })
            .unwrap_or(false);

    let album_artist_name = meta.album_artist.as_deref().unwrap_or_else(|| {
        if is_compilation {
            "Various Artists"
        } else {
            meta.artist.as_deref().unwrap_or("Unknown Artist")
        }
    });

    let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

    let album_artist_entry = artist_repo
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
    let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

    let track_artist = if is_compilation && track_artist_name != album_artist_name {
        artist_repo
            .get_or_create(track_artist_name, meta.musicbrainz_artist_id.as_deref(), None)
            .ok()
    } else {
        album_artist_entry.clone()
    };
    let artist_id = track_artist.as_ref().and_then(|a| a.id);

    let album = meta.album.as_ref().and_then(|title| {
        album_repo
            .get_or_create(title, album_artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
            .ok()
    });
    let album_id = album.as_ref().and_then(|a| a.id);

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

    Some((track, album_id))
}

/// Spawn the auto-scan task that indexes all music directories at startup.
pub fn spawn_auto_scan(db: SqliteDb, event_bus: Arc<EventBus>) {
    tokio::spawn(async move {
        info!("auto_scan_starting");
        let settings = tune_core::db::settings_repo::SettingsRepo::new(db.clone());
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

        let files = tune_core::scanner::walker::list_audio_files(&music_dirs);
        let total_discovered = files.len();
        info!(files = total_discovered, "auto_scan_files_found");

        let track_repo = TrackRepo::new(db.clone());
        let artist_repo = ArtistRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db.clone());

        let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_iter()
            .filter(|path| {
                let path_str = path.to_string_lossy();
                if let Some(&(_, existing_mtime, existing_size)) =
                    existing_tracks.get(path_str.as_ref())
                    && let Ok(file_meta) = path.metadata()
                {
                    let mtime = file_meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let unchanged = existing_mtime
                        .is_some_and(|m| (m - mtime as f64).abs() <= 0.5)
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

        let cache_dir = std::env::var("TUNE_ARTWORK_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("artwork_cache"));
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

                db.execute_batch("BEGIN IMMEDIATE").ok();

                for sf in &batch {
                    let Some((mut track, album_id)) =
                        build_track_from_metadata(sf, &artist_repo, &album_repo)
                    else {
                        continue;
                    };

                    if let Some(aid) = album_id
                        && !albums_with_cover.contains(&aid)
                        && let Some(hash) = tune_core::artwork::get_or_extract(
                            std::path::Path::new(&sf.path),
                            &cache_dir,
                        )
                    {
                        album_repo.update_cover_path(aid, &hash).ok();
                        albums_with_cover.insert(aid);
                    }

                    if let Some(&(existing_id, existing_mtime, existing_size)) =
                        existing_tracks.get(&sf.path)
                    {
                        let file_changed = existing_mtime
                            .is_none_or(|m| (m - sf.mtime as f64).abs() > 0.5)
                            || (existing_size != Some(sf.file_size as i64));

                        if !file_changed {
                            skipped += 1;
                            continue;
                        }

                        track.id = Some(existing_id);
                        to_update.push(track);
                        continue;
                    }

                    to_insert.push(track);
                }

                inserted += track_repo.create_batch(&to_insert).unwrap_or(0) as u64;
                updated += track_repo.update_batch(&to_update).unwrap_or(0) as u64;

                db.execute_batch("COMMIT").ok();
            },
        );

        for album in album_repo.list(99999, 0).unwrap_or_default() {
            if let Some(id) = album.id {
                album_repo.update_track_count(id).ok();
                album_repo.update_quality_from_tracks(id).ok();
            }
        }

        info!(
            total = stats.total_files,
            ok = stats.metadata_ok,
            failed = stats.metadata_failed,
            inserted,
            updated,
            skipped,
            artwork = albums_with_cover.len(),
            "auto_scan_complete"
        );

        event_bus.emit(
            "library.scan.completed",
            serde_json::json!({
                "total_files": stats.total_files,
                "metadata_ok": stats.metadata_ok,
                "metadata_failed": stats.metadata_failed,
                "inserted": inserted,
                "updated": updated,
                "skipped": skipped,
                "artwork_extracted": albums_with_cover.len(),
            }),
        );
    });
}

/// Spawn the file watcher that monitors music directories for live changes.
pub fn spawn_file_watcher(db: SqliteDb) {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(db.clone());
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
                loop {
                    let changes = watcher.poll_debounced(
                        std::time::Duration::from_secs(2),
                        std::time::Duration::from_millis(500),
                    );
                    for change in changes {
                        match change.change_type {
                            tune_core::scanner::watcher::ChangeType::Added
                            | tune_core::scanner::watcher::ChangeType::Modified => {
                                let files: Vec<std::path::PathBuf> =
                                    vec![std::path::PathBuf::from(&change.path)];
                                let (scanned, _) =
                                    tune_core::scanner::walker::scan_files_parallel(
                                        &files, true, None,
                                    );
                                let track_repo = TrackRepo::new(db.clone());
                                let artist_repo = ArtistRepo::new(db.clone());
                                let album_repo = AlbumRepo::new(db.clone());

                                for sf in &scanned {
                                    if sf.metadata.is_none() {
                                        continue;
                                    }

                                    if change.change_type
                                        == tune_core::scanner::watcher::ChangeType::Modified
                                    {
                                        track_repo.delete_by_path(&sf.path).ok();
                                    }

                                    let Some((track, album_id)) =
                                        build_track_from_metadata(sf, &artist_repo, &album_repo)
                                    else {
                                        continue;
                                    };

                                    if let Some(aid) = album_id {
                                        let cache_dir = std::env::var("TUNE_ARTWORK_DIR")
                                            .map(std::path::PathBuf::from)
                                            .unwrap_or_else(|_| {
                                                std::path::PathBuf::from("artwork_cache")
                                            });
                                        if let Some(hash) = tune_core::artwork::get_or_extract(
                                            std::path::Path::new(&sf.path),
                                            &cache_dir,
                                        ) {
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
                                let track_repo = TrackRepo::new(db.clone());
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
