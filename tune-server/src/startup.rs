use std::sync::Arc;

use tracing::info;

use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Restore zone volumes from DB and persist config settings to DB.
pub async fn init_state(state: &AppState, config: &TuneConfig) {
    restore_zone_volumes(state).await;
    persist_initial_settings(state, config);
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
