use std::sync::Arc;

use tracing::{info, warn};

use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Restore zone volumes and playback positions from DB, persist config settings.
pub async fn init_state(state: &AppState, config: &TuneConfig) {
    reset_zones_offline(state);
    deduplicate_zones(state);
    ensure_zones_is_hidden(state);
    cleanup_orphan_queues(state);
    deduplicate_radios(state);
    restore_zone_volumes(state).await;
    restore_playback_positions(state).await;
    restore_queues(state, config);
    restore_queue_metadata(state, config).await;
    restore_oaat_groups(state).await;
    persist_initial_settings(state, config);
    warm_sqlite_cache(state);

    // Re-register manually-added devices (BluOS, legacy DLNA renderers that
    // don't answer SSDP M-SEARCH). Done off the startup path so an offline
    // device's probe timeout doesn't delay boot.
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reregister_manual_devices(&state_clone).await;
    });
}

/// Reset all zones to offline at startup.  Discovery will set actually-present
/// devices back online.  This prevents stale "online" zones from accumulating
/// across restarts and hitting the free-tier zone limit.
fn reset_zones_offline(state: &AppState) {
    match state.backend.execute("UPDATE zones SET online = 0", &[]) {
        Ok(n) => {
            info!(count = n, "zones_reset_offline_at_startup");
        }
        Err(e) => {
            tracing::warn!(error = %e, "zones_reset_offline_failed");
        }
    }
}

/// Remove duplicate zones (same output_device_id) and add a unique index to
/// prevent future duplicates.  Must run before any discovery task starts.
fn deduplicate_zones(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    match zone_repo.deduplicate() {
        Ok(removed) if removed > 0 => {
            info!(removed, "zone_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "zone_dedup_failed");
        }
    }
    // Add a unique index on output_device_id (idempotent) so duplicate zones
    // can never be created again at the SQL level.
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_zones_output_device_id ON zones(output_device_id) WHERE output_device_id IS NOT NULL;"
    ) {
        tracing::warn!(error = %e, "zone_unique_index_failed");
    }
}

fn cleanup_orphan_queues(state: &AppState) {
    let sqls = [
        "DELETE FROM play_queue WHERE zone_id NOT IN (SELECT id FROM zones)",
        "DELETE FROM streaming_queue WHERE zone_id NOT IN (SELECT id FROM zones)",
    ];
    for sql in &sqls {
        match state.backend.execute(sql, &[]) {
            Ok(removed) if removed > 0 => {
                info!(removed, sql = *sql, "orphan_queue_records_cleaned");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "orphan_queue_cleanup_failed");
            }
        }
    }
}

fn ensure_zones_is_hidden(state: &AppState) {
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            // Try ALTER TABLE; ignore "duplicate column" error.
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN is_hidden INTEGER DEFAULT 0",
                &[],
            );
            match result {
                Ok(_) => info!("zones_is_hidden_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_is_hidden_column_add_failed"),
            }
        }
        tune_core::db::engine::Engine::Sqlite => {
            // Migration v38 handles this.
        }
    }

    // Ensure last_play_state column exists (migration v39 for SQLite,
    // idempotent ALTER for Postgres).
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN last_play_state TEXT DEFAULT 'stopped'",
                &[],
            );
            match result {
                Ok(_) => info!("zones_last_play_state_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_last_play_state_add_failed"),
            }
        }
        _ => {}
    }
}

fn deduplicate_radios(state: &AppState) {
    let dedup_sql = "DELETE FROM radio_stations WHERE id NOT IN (SELECT MIN(id) FROM radio_stations GROUP BY name, url)";
    match state.backend.execute(dedup_sql, &[]) {
        Ok(removed) if removed > 0 => {
            info!(removed, "radio_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "radio_dedup_failed");
        }
    }
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_radio_stations_name_url ON radio_stations(name, url);"
    ) {
        tracing::warn!(error = %e, "radio_unique_index_failed");
    }
}

/// Restore persisted queue snapshots from JSON files on disk.
fn restore_queues(state: &AppState, config: &TuneConfig) {
    tune_core::queue_persistence::restore_all_queues(&state.backend, &config.db_path);
}

/// After queues are restored into the DB, load snapshot metadata (repeat_mode,
/// shuffle, queue_length, current_position) into the PlaybackManager so the
/// poller's `next_position()` sees the correct values after a server restart.
async fn restore_queue_metadata(state: &AppState, config: &TuneConfig) {
    let snapshots = tune_core::queue_persistence::load_all_snapshots(&config.db_path);
    let queue_repo =
        tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone());

    for snap in &snapshots {
        let zone_id = snap.zone_id;

        // Determine queue length from DB (authoritative after restore_all_queues).
        let local_count = queue_repo.count(zone_id).unwrap_or(0);
        let streaming_count = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let queue_len = if local_count > 0 {
            local_count
        } else {
            streaming_count
        };

        if queue_len > 0 {
            state
                .playback
                .update_queue_info(zone_id, snap.current_position, queue_len)
                .await;
        }

        // Restore repeat mode
        let repeat = match snap.repeat_mode.as_str() {
            "one" => tune_core::playback::RepeatMode::One,
            "all" => tune_core::playback::RepeatMode::All,
            _ => tune_core::playback::RepeatMode::Off,
        };
        state.playback.set_repeat(zone_id, repeat).await;

        // Restore shuffle
        state.playback.set_shuffle(zone_id, snap.shuffle).await;

        info!(
            zone_id,
            queue_len,
            position = snap.current_position,
            repeat_mode = %snap.repeat_mode,
            shuffle = snap.shuffle,
            "queue_metadata_restored"
        );
    }
}

/// Touch key tables so SQLite page cache is warm for the first UI load.
fn warm_sqlite_cache(state: &AppState) {
    use tune_core::db::{album_repo::AlbumRepo, artist_repo::ArtistRepo, track_repo::TrackRepo};
    let _ = TrackRepo::with_backend(state.backend.clone()).count();
    let _ = AlbumRepo::with_backend(state.backend.clone()).count();
    let _ = ArtistRepo::with_backend(state.backend.clone()).count();
    info!("sqlite_cache_warmed");
}

/// Initialize PlaybackManager volume from DB-stored zone volumes and mark devices offline.
async fn restore_zone_volumes(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            if let Some(id) = zone.id {
                let vol = (zone.volume as f64) / 100.0;
                if vol >= 0.999 {
                    let safe_vol = 0.2;
                    state.playback.set_volume(id, safe_vol).await;
                    info!(zone_id = id, zone_name = %zone.name, volume = safe_vol, "zone_volume_clamped_from_100");
                } else {
                    state.playback.set_volume(id, vol).await;
                    info!(zone_id = id, zone_name = %zone.name, volume = vol, "zone_volume_restored");
                }
            }
        }
    }
}

/// Restore last playback positions from DB so the UI shows where playback left off.
async fn restore_playback_positions(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            let Some(zone_id) = zone.id else { continue };
            if zone.last_position_ms == 0
                && zone.last_track_id.is_none()
                && zone.last_track_source.as_deref() != Some("radio")
            {
                continue;
            }
            let np = if let Some(track_id) = zone.last_track_id {
                if let Ok(Some(track)) = track_repo.get(track_id) {
                    tune_core::playback::NowPlaying {
                        track_id: Some(track_id),
                        title: track.title.clone(),
                        artist_name: track.artist_name.clone(),
                        album_title: track.album_title.clone(),
                        cover_path: track.cover_path.clone(),
                        duration_ms: track.duration_ms,
                        source: zone
                            .last_track_source
                            .clone()
                            .unwrap_or_else(|| "local".into()),
                        source_id: zone.last_track_source_id.clone(),
                        stream_id: None,
                        format: track.format.clone(),
                        sample_rate: track.sample_rate.map(|v| v as u32),
                        bit_depth: track.bit_depth.map(|v| v as u32),
                        genre: track.genre.clone(),
                        year: track.year,
                    }
                } else {
                    continue;
                }
            } else if zone.last_track_source.as_deref() == Some("radio") {
                let radio_url = zone.last_track_source_id.clone().unwrap_or_default();
                tune_core::playback::NowPlaying {
                    track_id: None,
                    title: "Recovering...".into(),
                    artist_name: Some("Live Radio".into()),
                    album_title: Some("Live Radio".into()),
                    cover_path: None,
                    duration_ms: 0,
                    source: "radio".into(),
                    source_id: Some(radio_url),
                    stream_id: None,
                    ..Default::default()
                }
            } else {
                continue;
            };
            let clamped_pos = if np.duration_ms > 0 {
                zone.last_position_ms
                    .min(np.duration_ms.saturating_sub(1000))
            } else {
                zone.last_position_ms
            };
            let dur = np.duration_ms;
            state
                .playback
                .restore_position(zone_id, clamped_pos, np)
                .await;
            info!(
                zone_id,
                zone_name = %zone.name,
                position_ms = clamped_pos,
                original_ms = zone.last_position_ms,
                duration_ms = dur,
                track_id = ?zone.last_track_id,
                "playback_position_restored"
            );
        }
    }
}

/// Restore persisted OAAT multiroom groups from the settings DB.
#[cfg(feature = "oaat")]
async fn restore_oaat_groups(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups_json = settings
        .get("oaat_groups")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".into());
    let groups: Vec<serde_json::Value> = serde_json::from_str(&groups_json).unwrap_or_default();

    let mut restored = 0usize;
    for group in &groups {
        let id = match group["id"].as_str() {
            Some(id) => id.to_string(),
            None => continue,
        };
        let name = group["name"].as_str().unwrap_or("OAAT Group").to_string();
        let endpoints: Vec<(String, u16)> = group["endpoints"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|ep| {
                let host = ep["host"].as_str()?.to_string();
                let port = ep["port"].as_u64()? as u16;
                Some((host, port))
            })
            .collect();

        if endpoints.is_empty() {
            continue;
        }

        let output = tune_core::outputs::oaat::OaatMultiroomOutput::new(
            name.clone(),
            id.clone(),
            endpoints.clone(),
        );
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(output));
        drop(outputs);

        info!(group_id = %id, name = %name, endpoints = endpoints.len(), "oaat_group_restored");
        restored += 1;
    }

    if restored > 0 {
        info!(count = restored, "oaat_groups_restore_complete");
    }
}

#[cfg(not(feature = "oaat"))]
async fn restore_oaat_groups(_state: &AppState) {}

/// Create the OpenHome event listener (shared between SSDP handler and outputs).
pub async fn create_oh_listener() -> Option<Arc<OpenHomeEventListener>> {
    let server_ip = tune_core::discovery::ssdp::get_local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".into());
    match OpenHomeEventListener::new(server_ip).await {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            tracing::warn!(error = %e, "oh_event_listener_init_failed");
            None
        }
    }
}

/// Persist music_dirs and discogs_token from config/env into the settings DB.
fn persist_initial_settings(state: &AppState, config: &TuneConfig) {
    if !config.music_dirs.is_empty() {
        let normalized_dirs: Vec<String> = config
            .music_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(
                "music_dirs",
                &serde_json::to_string(&normalized_dirs).unwrap(),
            )
            .ok();
    }

    if let Some(ref token) = config.discogs_token {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
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
}

/// Register local audio output devices (USB DAC, headphones, speakers) and auto-create zones.
#[cfg(feature = "local-audio")]
pub async fn register_local_outputs(state: &AppState) {
    // Prefer DB-persisted backend (set via UI) over config/env default
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let db_backend = settings.get("local_audio_backend").ok().flatten();
    let audio_backend_owned =
        db_backend.unwrap_or_else(|| state.config.local_audio_backend.clone());
    let audio_backend = &audio_backend_owned;
    let mut devices = tune_core::outputs::local::list_audio_devices_with_backend(audio_backend);
    // When ASIO is selected but returns no devices, also enumerate WASAPI
    // so the user still has fallback outputs available.
    if devices.is_empty() && audio_backend.to_lowercase() == "asio" {
        warn!("asio_returned_no_devices — also enumerating WASAPI as fallback");
        devices = tune_core::outputs::local::list_audio_devices_with_backend("wasapi");
    }
    if !devices.is_empty() {
        let mut outputs = state.outputs.lock().await;
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());

        for dev in &devices {
            let device_id = format!("local:{}", dev.name);
            let local_out = tune_core::outputs::local::LocalOutput::with_options(
                dev.name.clone(),
                state.config.local_exclusive_mode,
                audio_backend,
            );
            outputs.register(Box::new(local_out));
            info!(
                name = %dev.name,
                device_id = %device_id,
                default = dev.is_default,
                channels = dev.max_channels,
                rates = ?dev.sample_rates,
                "local_audio_output_registered"
            );

            let zone_name = if dev.is_default {
                "This Computer".to_string()
            } else {
                dev.name.clone()
            };
            match zone_repo.get_or_create(&zone_name, Some("local"), &device_id) {
                Ok((zid, true)) => {
                    info!(
                        name = %zone_name,
                        zone_id = zid,
                        device_id = %device_id,
                        "local_audio_zone_auto_created"
                    );
                }
                Ok((_zid, false)) => {
                    let _ = zone_repo.set_online_by_device(&device_id, true);
                }
                Err(e) => {
                    tracing::warn!(
                        name = %zone_name,
                        device_id = %device_id,
                        error = %e,
                        "local_audio_zone_create_failed"
                    );
                }
            }
        }

        info!(count = devices.len(), "local_audio_devices_registered");
    } else {
        info!("no_local_audio_devices_found");
    }
}
