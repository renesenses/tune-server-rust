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
    let config = TuneConfig::load();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(format!("tune_server={}", config.log_level).parse().unwrap())
                .add_directive(format!("tune_core={}", config.log_level).parse().unwrap()),
        )
        .init();

    let state = AppState::new(&config.db_path, config.port).expect("failed to init app state");

    state.restore_tokens().await;

    // Initialize PlaybackManager volume from DB-stored zone volumes
    {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
        if let Ok(zones) = zone_repo.list() {
            for zone in &zones {
                if let Some(id) = zone.id {
                    let vol = (zone.volume as f64) / 100.0;
                    state.playback.set_volume(id, vol).await;
                    info!(zone_id = id, zone_name = %zone.name, volume = vol, "zone_volume_restored");
                }
            }
        }
    }

    if !config.music_dirs.is_empty() {
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        settings
            .set("music_dirs", &serde_json::to_string(&config.music_dirs).unwrap())
            .ok();
    }

    if config.auto_scan {
        let db = state.db.clone();
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

            for sf in &scanned {
                let Some(ref meta) = sf.metadata else {
                    continue;
                };

                let artist = artist_repo
                    .get_or_create(
                        meta.artist.as_deref().unwrap_or("Unknown Artist"),
                        meta.musicbrainz_artist_id.as_deref(),
                        meta.album_artist_sort.as_deref(),
                    )
                    .ok();
                let artist_id = artist.as_ref().and_then(|a| a.id);

                let album = meta.album.as_ref().and_then(|title| {
                    album_repo
                        .get_or_create(title, artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
                        .ok()
                });
                let album_id = album.as_ref().and_then(|a| a.id);

                if let Some(aid) = album_id {
                    if !albums_with_cover.contains(&aid) {
                        if let Some(hash) = tune_core::artwork::get_or_extract(
                            std::path::Path::new(&sf.path),
                            &cache_dir,
                        ) {
                            album_repo.update_cover_path(aid, &hash).ok();
                            albums_with_cover.insert(aid);
                        }
                    }
                }

                if track_repo.get_by_path(&sf.path).ok().flatten().is_some() {
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
                track.year = meta.year.map(|y| y as i32);
                track.label = meta.label.clone();
                track.isrc = meta.isrc.clone();
                track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();

                if track_repo.create(&track).is_ok() {
                    inserted += 1;
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
                artwork = albums_with_cover.len(),
                "auto_scan_complete"
            );
        });
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
        tokio::spawn(async move {
            use tune_core::discovery::ssdp::SsdpEvent;
            while let Some(event) = ssdp_rx.recv().await {
                match event {
                    SsdpEvent::DeviceDiscovered(dev) => {
                        let is_dlna = dev.device_type == tune_core::discovery::device::OutputType::Dlna
                            || dev.device_type == tune_core::discovery::device::OutputType::Openhome;
                        if is_dlna {
                            let svc_urls = dev.capabilities.get("service_urls")
                                .and_then(|v| serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok())
                                .unwrap_or_default();
                            let av_url = svc_urls.get("avtransport")
                                .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
                            let rc_url = svc_urls.get("renderingcontrol")
                                .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
                            if let (Some(av), Some(rc)) = (av_url, rc_url) {
                                let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    av,
                                    rc,
                                );
                                let mut reg = outputs.lock().await;
                                reg.register(Box::new(dlna));
                                info!(name = %dev.name, id = %dev.id, "dlna_output_registered");
                            }

                            let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(db_for_ssdp.clone());
                            let existing = zone_repo.list().unwrap_or_default();
                            let already = existing.iter().any(|z| z.output_device_id.as_deref() == Some(&dev.id));
                            if !already {
                                let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
                                let name_taken = existing.iter().any(|z| z.name == short_name);
                                let zone_name = if name_taken { dev.name.clone() } else { short_name.to_string() };
                                if let Ok(zid) = zone_repo.create(&zone_name, Some("dlna"), Some(&dev.id)) {
                                    info!(name = %zone_name, zone_id = zid, device = %dev.id, "zone_auto_created");
                                }
                            }
                        }
                    }
                    SsdpEvent::DeviceLost(id) => {
                        let mut reg = outputs.lock().await;
                        reg.remove(&id);
                        info!(id = %id, "output_removed");
                    }
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
                                        tune_core::db::settings_repo::SettingsRepo::new(
                                            db.clone(),
                                        );
                                    settings
                                        .set(
                                            &format!("auth_tokens_{name}"),
                                            &tokens.to_string(),
                                        )
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

    info!(
        version = tune_core::version(),
        port = config.port,
        db = %config.db_path,
        web = %config.web_dir,
        "tune_server_starting"
    );

    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    info!("shutdown_signal_received");
}
