use std::sync::Arc;

use tracing::info;

use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Restore zone volumes and playback positions from DB, persist config settings.
pub async fn init_state(state: &AppState, config: &TuneConfig) {
    restore_zone_volumes(state).await;
    restore_playback_positions(state).await;
    restore_oaat_groups(state).await;
    persist_initial_settings(state, config);
    warm_sqlite_cache(&state.db);
}

/// Touch key tables so SQLite page cache is warm for the first UI load.
fn warm_sqlite_cache(db: &tune_core::db::sqlite::SqliteDb) {
    use tune_core::db::{album_repo::AlbumRepo, artist_repo::ArtistRepo, track_repo::TrackRepo};
    let _ = TrackRepo::new(db.clone()).count();
    let _ = AlbumRepo::new(db.clone()).count();
    let _ = ArtistRepo::new(db.clone()).count();
    info!("sqlite_cache_warmed");
}

/// Initialize PlaybackManager volume from DB-stored zone volumes and mark devices offline.
async fn restore_zone_volumes(state: &AppState) {
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

/// Restore last playback positions from DB so the UI shows where playback left off.
async fn restore_playback_positions(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::new(state.db.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            let Some(zone_id) = zone.id else { continue };
            if zone.last_position_ms == 0 && zone.last_track_id.is_none() {
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
                    }
                } else {
                    continue;
                }
            } else {
                continue;
            };
            state
                .playback
                .restore_position(zone_id, zone.last_position_ms, np)
                .await;
            info!(
                zone_id,
                zone_name = %zone.name,
                position_ms = zone.last_position_ms,
                track_id = ?zone.last_track_id,
                "playback_position_restored"
            );
        }
    }
}

/// Restore persisted OAAT multiroom groups from the settings DB.
#[cfg(feature = "oaat")]
async fn restore_oaat_groups(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
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
        let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
        settings
            .set(
                "music_dirs",
                &serde_json::to_string(&normalized_dirs).unwrap(),
            )
            .ok();
    }

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
}

/// Register local audio output devices (USB DAC, headphones, speakers) and auto-create zones.
#[cfg(feature = "local-audio")]
pub async fn register_local_outputs(state: &AppState) {
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

            let already = existing_zones
                .iter()
                .any(|z| z.output_device_id.as_deref() == Some(&device_id));
            if !already {
                let zone_name = if dev.is_default {
                    "This Computer".to_string()
                } else {
                    dev.name.clone()
                };
                let name_taken = existing_zones.iter().any(|z| z.name == zone_name);
                if !name_taken
                    && let Ok(zid) = zone_repo.create(&zone_name, Some("local"), Some(&device_id))
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

        info!(count = devices.len(), "local_audio_devices_registered");
    } else {
        info!("no_local_audio_devices_found");
    }
}
