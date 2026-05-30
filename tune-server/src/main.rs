use tune_server::config;
use tune_server::routes;
use tune_server::state;

use std::net::SocketAddr;

use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::TuneConfig;
use crate::state::AppState;

#[tokio::main]
async fn main() {
    eprintln!("tune-server starting (pid {})", std::process::id());

    // Install rustls CryptoProvider before any TLS operation (reqwest, etc.)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls CryptoProvider");

    let config = TuneConfig::load();

    // Use local time for log timestamps (fixes UTC display on Windows/CEST systems).
    // Must capture offset before spawning threads (security restriction on some OS).
    let time_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let timer = tracing_subscriber::fmt::time::OffsetTime::new(
        time_offset,
        time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]"
        ),
    );

    tracing_subscriber::fmt()
        .with_timer(timer)
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("tune_server={}", config.log_level).parse().unwrap())
                .add_directive(format!("tune_core={}", config.log_level).parse().unwrap()),
        )
        .init();

    let state = AppState::new(&config.db_path, config.port, config.clone())
        .expect("failed to init app state");

    state.restore_tokens().await;

    // Initialize PlaybackManager volume from DB-stored zone volumes
    {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
        if let Ok(zones) = zone_repo.list() {
            for zone in &zones {
                if let Some(id) = zone.id {
                    let vol = (zone.volume as f64) / 100.0;
                    state.playback.set_volume(id, vol).await;
                    if zone.output_device_id.is_some() {
                        let _ = zone_repo.update_online(id, false);
                    }
                    info!(zone_id = id, zone_name = %zone.name, volume = vol, "zone_volume_restored");
                }
            }
        }
    }

    if !config.music_dirs.is_empty() {
        // Normalize paths before persisting (handles Windows backslashes, trailing separators, etc.)
        let normalized_dirs: Vec<String> = config
            .music_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        settings
            .set(
                "music_dirs",
                &serde_json::to_string(&normalized_dirs).unwrap(),
            )
            .ok();
    }

    // Persist TUNE_DISCOGS_TOKEN from env/.env to settings DB if not already configured
    if let Some(ref token) = config.discogs_token {
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        let already_set = settings
            .get("discogs_token")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !already_set {
            settings.set("discogs_token", token).ok();
            info!("discogs_token_persisted_from_env");
        }
    }

    if config.auto_scan {
        let db = state.db.clone();
        let event_bus = state.event_bus.clone();
        tokio::spawn(async move {
            info!("auto_scan_starting");
            let settings = tune_core::db::settings_repo::SettingsRepo::new(db.clone());
            let raw_dirs: Vec<String> = settings
                .get("music_dirs")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

            // Normalize paths for cross-platform compatibility
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

            let track_repo = tune_core::db::track_repo::TrackRepo::new(db.clone());
            let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(db.clone());
            let album_repo = tune_core::db::album_repo::AlbumRepo::new(db.clone());

            // Load all existing local tracks in one query for efficient change detection
            let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

            // Pre-filter: skip files whose mtime+size haven't changed
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

            // Batched scan: parse metadata in parallel, batch-insert per chunk
            let stats = tune_core::scanner::walker::scan_files_batched(
                &files_to_scan,
                true,
                tune_core::scanner::walker::SCAN_BATCH_SIZE,
                |batch, _batch_idx, _total_files| {
                    let mut to_insert: Vec<tune_core::db::models::Track> =
                        Vec::with_capacity(batch.len());
                    let mut to_update: Vec<tune_core::db::models::Track> =
                        Vec::with_capacity(batch.len() / 4);

                    db.execute_batch("BEGIN IMMEDIATE").ok();

                    for sf in &batch {
                        let Some(ref meta) = sf.metadata else {
                            continue;
                        };

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

                        let track_artist =
                            if is_compilation && track_artist_name != album_artist_name {
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

                        let album = meta.album.as_ref().and_then(|title| {
                            album_repo
                                .get_or_create(
                                    title,
                                    album_artist_id.unwrap_or(0),
                                    meta.year.map(|y| y as i32),
                                )
                                .ok()
                        });
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
                        }

                        let title = meta.title.clone().unwrap_or_else(|| {
                            std::path::Path::new(&sf.path)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default()
                        });

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
                            track.year = meta.year.map(|y| y as i32);
                            track.label = meta.label.clone();
                            track.isrc = meta.isrc.clone();
                            track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
                            to_update.push(track);
                            continue;
                        }

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
                        track.year = meta.year.map(|y| y as i32);
                        track.label = meta.label.clone();
                        track.isrc = meta.isrc.clone();
                        track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
                        to_insert.push(track);
                    }

                    // Batch insert + update using prepared statements
                    inserted += track_repo.create_batch(&to_insert).unwrap_or(0) as u64;
                    updated += track_repo.update_batch(&to_update).unwrap_or(0) as u64;

                    // COMMIT this batch -- tracks are now queryable
                    db.execute_batch("COMMIT").ok();
                },
            );

            // Update album stats
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

    // File watcher: watch music dirs and rescan/remove tracks on changes
    {
        let db_for_watcher = state.db.clone();
        let settings = tune_core::db::settings_repo::SettingsRepo::new(db_for_watcher.clone());
        let music_dirs: Vec<String> = settings
            .get("music_dirs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        if !music_dirs.is_empty() {
            match tune_core::scanner::watcher::FileWatcher::new(music_dirs) {
                Ok(watcher) => {
                    info!("file_watcher_started");
                    tokio::task::spawn_blocking(move || {
                        let db = db_for_watcher;
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
                                        let track_repo =
                                            tune_core::db::track_repo::TrackRepo::new(db.clone());
                                        let artist_repo =
                                            tune_core::db::artist_repo::ArtistRepo::new(db.clone());
                                        let album_repo =
                                            tune_core::db::album_repo::AlbumRepo::new(db.clone());

                                        for sf in &scanned {
                                            let Some(ref meta) = sf.metadata else {
                                                continue;
                                            };

                                            // Remove existing entry if modified
                                            if change.change_type
                                                == tune_core::scanner::watcher::ChangeType::Modified
                                            {
                                                track_repo.delete_by_path(&sf.path).ok();
                                            }

                                            // Determine if this is a compilation
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

                                            // Album artist: use album_artist tag, fall back to track artist
                                            let album_artist_name =
                                                meta.album_artist.as_deref().unwrap_or_else(|| {
                                                    meta.artist
                                                        .as_deref()
                                                        .unwrap_or("Unknown Artist")
                                                });

                                            let track_artist_name =
                                                meta.artist.as_deref().unwrap_or("Unknown Artist");

                                            // For the album, use album_artist (so compilations group under "Various Artists")
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
                                            let album_artist_id =
                                                album_artist_entry.as_ref().and_then(|a| a.id);

                                            // For the track, use track-level artist (important for compilations)
                                            let track_artist = if is_compilation
                                                && track_artist_name != album_artist_name
                                            {
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
                                            let artist_id =
                                                track_artist.as_ref().and_then(|a| a.id);

                                            let album = meta.album.as_ref().and_then(|title| {
                                                album_repo
                                                    .get_or_create(
                                                        title,
                                                        album_artist_id.unwrap_or(0),
                                                        meta.year.map(|y| y as i32),
                                                    )
                                                    .ok()
                                            });
                                            let album_id = album.as_ref().and_then(|a| a.id);

                                            if let Some(aid) = album_id {
                                                let cache_dir = std::env::var("TUNE_ARTWORK_DIR")
                                                    .map(std::path::PathBuf::from)
                                                    .unwrap_or_else(|_| {
                                                        std::path::PathBuf::from("artwork_cache")
                                                    });
                                                if let Some(hash) =
                                                    tune_core::artwork::get_or_extract(
                                                        std::path::Path::new(&sf.path),
                                                        &cache_dir,
                                                    )
                                                {
                                                    album_repo.update_cover_path(aid, &hash).ok();
                                                }
                                                album_repo.update_track_count(aid).ok();
                                                album_repo.update_quality_from_tracks(aid).ok();
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
                                            track.artist_name = Some(track_artist_name.to_string());
                                            track.album_artist = meta.album_artist.clone();
                                            track.album_title = meta.album.clone();
                                            track.disc_number =
                                                meta.disc_number.unwrap_or(1) as i32;
                                            track.track_number =
                                                meta.track_number.unwrap_or(0) as i32;
                                            track.duration_ms =
                                                meta.duration_ms.unwrap_or(0) as i64;
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
                                            track.musicbrainz_recording_id =
                                                meta.musicbrainz_recording_id.clone();

                                            if track_repo.create(&track).is_ok() {
                                                info!(path = %sf.path, "watcher_track_added");
                                            }
                                        }
                                    }
                                    tune_core::scanner::watcher::ChangeType::Deleted => {
                                        let track_repo =
                                            tune_core::db::track_repo::TrackRepo::new(db.clone());
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
    }

    // Register local audio output devices (USB DAC, headphones, speakers)
    #[cfg(feature = "local-audio")]
    {
        let devices = tune_core::outputs::local::list_audio_devices();
        if !devices.is_empty() {
            let mut outputs = state.outputs.lock().await;
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
            let existing_zones = zone_repo.list().unwrap_or_default();

            for dev in &devices {
                let device_id = format!("local:{}", dev.name);
                let local_out = tune_core::outputs::local::LocalOutput::new(dev.name.clone());
                outputs.register(Box::new(local_out));
                info!(
                    name = %dev.name,
                    device_id = %device_id,
                    default = dev.is_default,
                    channels = dev.max_channels,
                    rates = ?dev.sample_rates,
                    "local_audio_output_registered"
                );

                // Auto-create zone if not already mapped to this device
                let already = existing_zones
                    .iter()
                    .any(|z| z.output_device_id.as_deref() == Some(&device_id));
                if !already {
                    let zone_name = if dev.is_default {
                        "This Computer".to_string()
                    } else {
                        dev.name.clone()
                    };
                    // Skip if a zone with that name already exists
                    let name_taken = existing_zones.iter().any(|z| z.name == zone_name);
                    if !name_taken {
                        if let Ok(zid) =
                            zone_repo.create(&zone_name, Some("local"), Some(&device_id))
                        {
                            info!(
                                name = %zone_name,
                                zone_id = zid,
                                device_id = %device_id,
                                "local_audio_zone_auto_created"
                            );
                        }
                    }
                }
            }

            info!(count = devices.len(), "local_audio_devices_registered");
        } else {
            info!("no_local_audio_devices_found");
        }
    }

    let oh_event_listener = {
        let server_ip = tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        match tune_core::outputs::oh_events::OpenHomeEventListener::new(server_ip).await {
            Ok(l) => Some(std::sync::Arc::new(l)),
            Err(e) => {
                tracing::warn!(error = %e, "oh_event_listener_init_failed");
                None
            }
        }
    };

    {
        let (ssdp_tx, mut ssdp_rx) = tokio::sync::mpsc::channel(64);
        {
            let mut scanner = state.scanner.lock().await;
            *scanner = tune_core::discovery::ssdp::SsdpScanner::new(ssdp_tx);
            scanner.start().await;
        }

        let outputs = state.outputs.clone();
        let db_for_ssdp = state.db.clone();
        let config_for_ssdp = config.clone();
        let event_bus_for_ssdp = state.event_bus.clone();
        let oh_listener_for_ssdp = oh_event_listener.clone();
        tokio::spawn(async move {
            use tune_core::discovery::ssdp::SsdpEvent;
            while let Some(event) = ssdp_rx.recv().await {
                match event {
                    SsdpEvent::DeviceDiscovered(dev) => {
                        let is_renderer = dev.device_type
                            == tune_core::discovery::device::OutputType::Dlna
                            || dev.device_type
                                == tune_core::discovery::device::OutputType::Openhome;
                        if is_renderer {
                            let svc_urls = dev
                                .capabilities
                                .get("service_urls")
                                .and_then(|v| {
                                    serde_json::from_value::<
                                        std::collections::HashMap<String, String>,
                                    >(v.clone())
                                    .ok()
                                })
                                .unwrap_or_default();

                            if dev.device_type == tune_core::discovery::device::OutputType::Openhome
                            {
                                let evt_urls = dev
                                    .capabilities
                                    .get("event_sub_urls")
                                    .and_then(|v| {
                                        serde_json::from_value::<
                                            std::collections::HashMap<String, String>,
                                        >(v.clone())
                                        .ok()
                                    })
                                    .unwrap_or_default();
                                let oh = tune_core::outputs::openhome::OpenHomeOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                    svc_urls.clone(),
                                    oh_listener_for_ssdp.clone(),
                                    evt_urls,
                                );
                                let mut reg = outputs.lock().await;
                                reg.register(Box::new(oh));
                                info!(name = %dev.name, id = %dev.id, "openhome_output_registered");
                            } else {
                                let av_url = svc_urls
                                    .get("avtransport")
                                    .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
                                let rc_url = svc_urls
                                    .get("renderingcontrol")
                                    .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
                                if let (Some(av), Some(rc)) = (av_url, rc_url) {
                                    let delay = config_for_ssdp.play_delay_for(&dev.name);
                                    let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                                        dev.name.clone(),
                                        dev.id.clone(),
                                        dev.host.clone(),
                                        av,
                                        rc,
                                    )
                                    .with_play_delay(delay);
                                    let mut reg = outputs.lock().await;
                                    reg.register(Box::new(dlna));
                                    info!(name = %dev.name, id = %dev.id, "dlna_output_registered");
                                }
                            }

                            let skip_keywords = [
                                "tv",
                                "décodeur",
                                "decoder",
                                "kdl-",
                                "bravia",
                                "samsung",
                                "lg ",
                                "philips tv",
                                "chromecast",
                            ];
                            let name_lower = dev.name.to_lowercase();
                            let is_tv = skip_keywords.iter().any(|kw| name_lower.contains(kw));

                            let zone_repo =
                                tune_core::db::zone_repo::ZoneRepo::new(db_for_ssdp.clone());
                            let existing = zone_repo.list().unwrap_or_default();
                            let already = existing
                                .iter()
                                .any(|z| z.output_device_id.as_deref() == Some(&dev.id));
                            if already {
                                let _ = zone_repo.set_online_by_device(&dev.id, true);
                                info!(name = %dev.name, id = %dev.id, "zone_device_reconnected");
                                event_bus_for_ssdp.emit(
                                    "device.reconnected",
                                    serde_json::json!({
                                        "device_id": &dev.id,
                                        "name": &dev.name,
                                        "host": &dev.host,
                                    }),
                                );
                            } else if !is_tv {
                                let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
                                let name_taken = existing.iter().any(|z| z.name == short_name);
                                let zone_name = if name_taken {
                                    dev.name.clone()
                                } else {
                                    short_name.to_string()
                                };
                                let type_str = if dev.device_type
                                    == tune_core::discovery::device::OutputType::Openhome
                                {
                                    "openhome"
                                } else {
                                    "dlna"
                                };
                                if let Ok(zid) =
                                    zone_repo.create(&zone_name, Some(type_str), Some(&dev.id))
                                {
                                    info!(name = %zone_name, zone_id = zid, device = %dev.id, r#type = type_str, "zone_auto_created");
                                }
                            }
                        }
                    }
                    SsdpEvent::DeviceLost(id) => {
                        let mut reg = outputs.lock().await;
                        reg.remove(&id);
                        let zone_repo =
                            tune_core::db::zone_repo::ZoneRepo::new(db_for_ssdp.clone());
                        let _ = zone_repo.set_online_by_device(&id, false);
                        info!(id = %id, "output_removed_zone_offline");
                    }
                }
            }
        });
    }

    let _mdns_handle;
    {
        let (mdns_tx, mut mdns_rx) = tokio::sync::mpsc::channel(64);
        _mdns_handle = if let Ok(mdns) = tune_core::discovery::mdns::MdnsScanner::new(mdns_tx) {
            let mut mdns = mdns
                .with_chromecast()
                .with_airplay()
                .with_bluos()
                .with_oaat()
                .with_squeezebox();
            if let Err(e) = mdns.start() {
                tracing::warn!(error = %e, "mdns_start_failed");
            }
            Some(mdns)
        } else {
            None
        };

        let outputs = state.outputs.clone();
        let db_for_mdns = state.db.clone();
        tokio::spawn(async move {
            use tune_core::discovery::device::OutputType;
            use tune_core::discovery::mdns::MdnsEvent;
            while let Some(event) = mdns_rx.recv().await {
                match event {
                    MdnsEvent::DeviceDiscovered(dev) | MdnsEvent::DeviceUpdated(dev) => {
                        let (output, output_type_str): (
                            Option<Box<dyn tune_core::outputs::OutputTarget>>,
                            &str,
                        ) = match dev.device_type {
                            OutputType::Chromecast => {
                                let cast = tune_core::outputs::chromecast::ChromecastOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                );
                                (Some(Box::new(cast)), "chromecast")
                            }
                            OutputType::Airplay => {
                                let ap = tune_core::outputs::airplay::AirplayOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                );
                                (Some(Box::new(ap)), "airplay")
                            }
                            OutputType::Bluos => {
                                let bluos = tune_core::outputs::bluos::BluosOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                );
                                (Some(Box::new(bluos)), "bluos")
                            }
                            OutputType::Oaat => {
                                let oaat = tune_core::outputs::oaat::OaatOutput::new(
                                    dev.name.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                    dev.id.clone(),
                                );
                                (Some(Box::new(oaat)), "oaat")
                            }
                            OutputType::Squeezebox => {
                                // mDNS _slimcli._tcp discovers the LMS server (CLI port 9090).
                                // Store the host for LMS polling; JSON-RPC uses port 9000.
                                let settings = tune_core::db::settings_repo::SettingsRepo::new(
                                    db_for_mdns.clone(),
                                );
                                let current = settings
                                    .get("squeezebox_host")
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default();
                                if current.is_empty() {
                                    let lms_addr = format!("{}:9000", dev.host);
                                    settings.set("squeezebox_host", &lms_addr).ok();
                                    settings.set("squeezebox_enabled", "true").ok();
                                    info!(host = %lms_addr, "mdns_lms_discovered_auto_configured");
                                }
                                // Individual players are discovered via LMS polling, not mDNS
                                (None, "squeezebox")
                            }
                            _ => (None, ""),
                        };

                        if let Some(output) = output {
                            let mut reg = outputs.lock().await;
                            reg.register(output);
                            info!(name = %dev.name, host = %dev.host, port = dev.port, r#type = output_type_str, "mdns_output_registered");

                            let zone_repo =
                                tune_core::db::zone_repo::ZoneRepo::new(db_for_mdns.clone());
                            let existing = zone_repo.list().unwrap_or_default();
                            let already_by_device = existing
                                .iter()
                                .any(|z| z.output_device_id.as_deref() == Some(&dev.id));
                            if already_by_device {
                                let _ = zone_repo.set_online_by_device(&dev.id, true);
                                info!(name = %dev.name, id = %dev.id, "mdns_zone_reconnected");
                            } else {
                                // Skip zone creation if a zone with the same name already exists
                                // (e.g., DLNA already created "DMP-A8", skip AirPlay duplicate)
                                let name_taken = existing.iter().any(|z| z.name == dev.name);
                                if !name_taken {
                                    if let Ok(zid) = zone_repo.create(
                                        &dev.name,
                                        Some(output_type_str),
                                        Some(&dev.id),
                                    ) {
                                        info!(name = %dev.name, zone_id = zid, r#type = output_type_str, "mdns_zone_auto_created");
                                    }
                                } else {
                                    info!(name = %dev.name, r#type = output_type_str, "mdns_zone_skipped_name_exists");
                                }
                            }
                        }
                    }
                    MdnsEvent::DeviceLost(id) => {
                        let mut reg = outputs.lock().await;
                        reg.remove(&id);
                    }
                }
            }
        });
    }

    // Squeezebox/LMS player discovery: poll LMS for players when squeezebox_enabled=true
    {
        let state_for_sb = state.clone();
        tokio::spawn(async move {
            // Initial delay to let the server fully start
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::new(state_for_sb.db.clone());
                let enabled = settings
                    .get("squeezebox_enabled")
                    .ok()
                    .flatten()
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(false);
                let host = settings
                    .get("squeezebox_host")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if enabled && !host.is_empty() {
                    match crate::routes::squeezebox::discover_and_register(&state_for_sb).await {
                        Ok(players) => {
                            if !players.is_empty() {
                                info!(count = players.len(), lms = %host, "squeezebox_poll_discovered");
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, lms = %host, "squeezebox_poll_failed");
                        }
                    }
                }

                // Poll every 60 seconds
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    // Periodic cleanup of stale streaming sessions (prevents memory leaks)
    {
        let streamer = state.streamer.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                ticker.tick().await;
                let removed = streamer.cleanup_stale_sessions().await;
                if removed > 0 {
                    info!(removed, "session_gc_sweep");
                }
            }
        });
    }

    let poller = tune_core::poller::PositionPoller::new(
        state.orchestrator.clone(),
        state.playback.clone(),
        state.outputs.clone(),
        state.db.clone(),
    );
    poller.spawn();

    {
        let services = state.services.clone();
        let db = state.db.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                ticker.tick().await;
                let registry = services.lock().await;
                for name in registry.list() {
                    if let Some(svc) = registry.get(&name) {
                        let mut svc = svc.lock().await;
                        match svc.refresh_if_needed().await {
                            Ok(true) => {
                                if let Some(tokens) = svc.save_tokens() {
                                    let settings =
                                        tune_core::db::settings_repo::SettingsRepo::new(db.clone());
                                    settings
                                        .set(&format!("auth_tokens_{name}"), &tokens.to_string())
                                        .ok();
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!(service = %name, error = %e, "token_refresh_failed");
                            }
                        }
                    }
                }
            }
        });
    }

    // Start UPnP MediaServer SSDP advertiser
    if let Some(ref upnp) = state.upnp {
        let location = format!(
            "http://{}:{}/upnp/description.xml",
            tune_core::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into()),
            config.port,
        );
        tune_core::upnp_server::spawn_ssdp_advertiser(upnp.uuid.clone(), location).await;
        info!("upnp_mediaserver_advertiser_started");
    }

    // Configure Deezer proxy base URL so get_track_url returns decryptable URLs
    {
        let registry = state.services.lock().await;
        if let Some(svc) = registry.get("deezer") {
            let mut svc = svc.lock().await;
            if let Some(deezer) = svc
                .as_any_mut()
                .downcast_mut::<tune_core::streaming::deezer::DeezerService>()
            {
                let server_ip = tune_core::discovery::ssdp::get_local_ip()
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "127.0.0.1".into());
                deezer.set_proxy_base_url(Some(format!(
                    "http://{}:{}/deezer-proxy",
                    server_ip, config.port
                )));
                info!("deezer_proxy_configured");
            }
        }
    }

    // Alarm scheduler
    {
        let alarm_sched = std::sync::Arc::new(tune_core::alarms::AlarmScheduler::new(
            state.db.clone(),
            state.orchestrator.clone(),
        ));
        alarm_sched.spawn();
    }

    // Desktop notifications
    if tune_core::notifications::is_enabled() {
        let server_ip = tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let server_base = std::sync::Arc::new(format!("http://{}:{}", server_ip, config.port));
        tune_core::notifications::spawn_notification_listener(
            state.event_bus.subscribe(),
            server_base,
        );
    }

    state.event_bus.emit(
        "system.started",
        serde_json::json!({
            "version": tune_core::version(),
            "port": config.port,
        }),
    );

    info!(
        version = tune_core::version(),
        port = config.port,
        db = %config.db_path,
        web = %config.web_dir,
        "tune_server_starting"
    );

    let outputs_for_diag = state.outputs.clone();
    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                tracing::warn!(addr = %addr, error = %e, "port_busy_retrying");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    // Periodic RSS memory diagnostics (Linux only)
    {
        let outputs = outputs_for_diag;
        tokio::spawn(async move {
            loop {
                #[cfg(target_os = "linux")]
                if let Ok(statm) = tokio::fs::read_to_string("/proc/self/statm").await {
                    let rss_pages: u64 = statm
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let rss_mb = rss_pages * 4 / 1024;
                    let count = outputs.lock().await.list().len();
                    info!(rss_mb, outputs_count = count, "memory_diagnostics");
                }
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        });
    }

    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await.expect("failed to install CTRL+C handler");

    info!("shutdown_signal_received");

    // Force exit after 3s if graceful shutdown stalls — must use std::thread
    // because tokio runtime may itself be stalling
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        eprintln!("shutdown_timeout_forcing_exit");
        std::process::exit(0);
    });
}
