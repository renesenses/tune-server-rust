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

    tracing_subscriber::fmt()
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
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        settings
            .set(
                "music_dirs",
                &serde_json::to_string(&config.music_dirs).unwrap(),
            )
            .ok();
    }

    if config.auto_scan {
        let db = state.db.clone();
        let event_bus = state.event_bus.clone();
        tokio::spawn(async move {
            info!("auto_scan_starting");
            let settings = tune_core::db::settings_repo::SettingsRepo::new(db.clone());
            let music_dirs: Vec<String> = settings
                .get("music_dirs")
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

            if music_dirs.is_empty() {
                info!("auto_scan_skipped_no_dirs");
                return;
            }

            let files = tune_core::scanner::walker::list_audio_files(&music_dirs);
            info!(files = files.len(), "auto_scan_files_found");

            event_bus.emit(
                "library.scan.started",
                serde_json::json!({
                    "music_dirs": &music_dirs,
                    "total": files.len(),
                    "auto": true,
                }),
            );

            let (scanned, stats) =
                tune_core::scanner::walker::scan_files_parallel(&files, true, None);

            let track_repo = tune_core::db::track_repo::TrackRepo::new(db.clone());
            let artist_repo = tune_core::db::artist_repo::ArtistRepo::new(db.clone());
            let album_repo = tune_core::db::album_repo::AlbumRepo::new(db.clone());

            let cache_dir = std::env::var("TUNE_ARTWORK_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("artwork_cache"));
            let mut albums_with_cover: std::collections::HashSet<i64> =
                std::collections::HashSet::new();
            let mut inserted = 0u64;
            let mut updated = 0u64;
            let mut skipped = 0u64;
            let total = scanned.len() as u64;
            let mut last_progress_emit = std::time::Instant::now();

            // Load all existing local tracks in one query for efficient change detection
            let existing_tracks = track_repo.get_all_local_file_info().unwrap_or_default();

            for sf in &scanned {
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

                // Album artist: use album_artist tag, fall back to "Various Artists" for compilations
                let album_artist_name = meta.album_artist.as_deref().unwrap_or_else(|| {
                    if is_compilation {
                        "Various Artists"
                    } else {
                        meta.artist.as_deref().unwrap_or("Unknown Artist")
                    }
                });

                // Track artist: always from track-level artist tag
                let track_artist_name = meta.artist.as_deref().unwrap_or("Unknown Artist");

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

                // Build the title (shared between insert and update)
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
                    // File exists — check if it has changed (different mtime or size)
                    let file_changed = existing_mtime
                        .map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
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
                    track.year = meta.year.map(|y| y as i32);
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
                track.year = meta.year.map(|y| y as i32);
                track.label = meta.label.clone();
                track.isrc = meta.isrc.clone();
                track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();

                if track_repo.create(&track).is_ok() {
                    inserted += 1;
                }

                // Emit progress every 500 processed files or every 2 seconds
                let processed = inserted + updated + skipped;
                let elapsed = last_progress_emit.elapsed();
                if processed > 0
                    && (processed % 500 == 0 || elapsed >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();
                    event_bus.emit(
                        "library.scan.progress",
                        serde_json::json!({
                            "scanned": processed,
                            "total": total,
                            "current_file": sf.path,
                            "inserted": inserted,
                            "updated": updated,
                            "skipped": skipped,
                        }),
                    );
                }
            }

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

                            {
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
                                let type_str = if dev.device_type == tune_core::discovery::device::OutputType::Openhome { "openhome" } else { "dlna" };
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
            let mut mdns = mdns.with_chromecast().with_airplay().with_bluos();
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
                        let (output, output_type_str): (Option<Box<dyn tune_core::outputs::OutputTarget>>, &str) =
                            match dev.device_type {
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
                                    info!(name = %dev.name, host = %dev.host, "bluos_device_discovered");
                                    (None, "bluos")
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
                            let already = existing
                                .iter()
                                .any(|z| z.output_device_id.as_deref() == Some(&dev.id));
                            if !already
                                && let Ok(zid) =
                                    zone_repo.create(&dev.name, Some(output_type_str), Some(&dev.id))
                            {
                                info!(name = %dev.name, zone_id = zid, r#type = output_type_str, "mdns_zone_auto_created");
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
}
