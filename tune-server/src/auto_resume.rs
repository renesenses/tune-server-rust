use tracing::{debug, info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::PlayRequest;

use crate::state::AppState;

/// Maximum staleness (seconds) of `server_last_alive_at` before we consider
/// the server to have crashed.  If the gap is smaller (clean restart, quick
/// bounce) we still attempt auto-resume.
const STALENESS_THRESHOLD_SECS: u64 = 3600;

/// Deadline (seconds) for waiting for network devices to reconnect after a
/// server restart before giving up on auto-resume.
const RECONNECT_DEADLINE_SECS: u64 = 120;

/// Returns `true` if the server was down for longer than the staleness
/// threshold (likely a crash or long outage where resuming makes no sense).
fn server_was_down_too_long(state: &AppState) -> bool {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let last_alive = match settings.get("server_last_alive_at").ok().flatten() {
        Some(ts) => match ts.parse::<u64>() {
            Ok(v) => v,
            Err(_) => return true, // unparseable = treat as stale
        },
        None => return true, // never set = first run, nothing to resume
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let gap = now.saturating_sub(last_alive);
    if gap > STALENESS_THRESHOLD_SECS {
        info!(
            gap_secs = gap,
            threshold = STALENESS_THRESHOLD_SECS,
            "auto_resume_skipped_server_down_too_long"
        );
        true
    } else {
        debug!(gap_secs = gap, "auto_resume_staleness_ok");
        false
    }
}

/// Attempt to resume playback on a single zone.  Builds a PlayRequest from
/// the zone's persisted NowPlaying / queue state and calls orchestrator.play().
/// After play succeeds, seeks to the last known position.
///
/// Returns `true` if playback was successfully resumed.
async fn try_auto_resume_zone(state: &AppState, zone_id: i64) -> bool {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => return false,
    };

    // A live radio stream must never auto-restart on server boot. The user
    // pressed stop (or simply quit), and re-launching a radio on every startup
    // with no interaction is the "phantom playback that survives restart and
    // can't be killed" bug: it comes back each boot on the local zone. Real
    // tracks resume fine; radio is a continuous live source, so we don't.
    if zone.last_track_source.as_deref() == Some("radio") {
        debug!(zone_id, zone_name = %zone.name, "auto_resume_skip_radio");
        return false;
    }

    // Need at least a track id or a source+source_id to resume
    let has_track = zone.last_track_id.is_some();
    let has_streaming = zone.last_track_source.is_some() && zone.last_track_source_id.is_some();
    if !has_track && !has_streaming {
        debug!(zone_id, "auto_resume_no_track_info");
        return false;
    }

    let req = PlayRequest {
        zone_id,
        output_device_id: zone.output_device_id.clone(),
        track_id: zone.last_track_id,
        source: zone.last_track_source.clone(),
        source_id: zone.last_track_source_id.clone(),
        title: None,
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
    };

    // Auto-resume must not block on a slow track resolution. Login-gated
    // YouTube falls back to yt-dlp (30-90s), so a resumed YouTube track would
    // start audio long after boot — superimposing on whatever the user launched
    // meanwhile ("lecture double", Jean Marie: a background zone's YouTube track
    // resumed ~1 min late over a radio he'd started). Give resolution a short
    // deadline; if the track can't start promptly, abandon the resume rather
    // than fire it late. Local files and Tidal/Qobuz resolve in a few seconds.
    const AUTO_RESUME_RESOLVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    match tokio::time::timeout(AUTO_RESUME_RESOLVE_TIMEOUT, state.orchestrator.play(req)).await {
        Ok(Ok(_result)) => {
            // Seek to the last known position
            let position_ms = zone.last_position_ms;
            if position_ms > 0 {
                let device_id = zone.output_device_id.as_deref();
                state
                    .orchestrator
                    .seek(zone_id, position_ms as u64, device_id)
                    .await;
            }
            info!(
                zone_id,
                zone_name = %zone.name,
                position_ms,
                "auto_resume_zone_success"
            );
            true
        }
        Ok(Err(e)) => {
            warn!(zone_id, zone_name = %zone.name, error = %e, "auto_resume_zone_failed");
            false
        }
        Err(_) => {
            warn!(
                zone_id,
                zone_name = %zone.name,
                timeout_secs = AUTO_RESUME_RESOLVE_TIMEOUT.as_secs(),
                "auto_resume_resolve_timeout_abandoned"
            );
            false
        }
    }
}

/// For zones using `local:*` devices, resume immediately without waiting for
/// network discovery events (the local output is already registered).
pub async fn auto_resume_local_zones(state: &AppState) {
    if server_was_down_too_long(state) {
        return;
    }

    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = match zone_repo.list() {
        Ok(z) => z,
        Err(_) => return,
    };

    for zone in &zones {
        let Some(zone_id) = zone.id else { continue };
        let Some(ref device_id) = zone.output_device_id else {
            continue;
        };
        if !device_id.starts_with("local:") {
            continue;
        }
        let play_state = zone_repo.get_last_play_state(zone_id);
        if play_state.as_deref() != Some("playing") {
            continue;
        }
        info!(zone_id, zone_name = %zone.name, "auto_resume_local_zone_attempting");
        try_auto_resume_zone(state, zone_id).await;
    }
}

/// Spawn a background task that listens for `device.reconnected` events and
/// auto-resumes zones whose last_play_state was "playing".
///
/// The listener runs for up to `RECONNECT_DEADLINE_SECS` seconds, then exits.
/// Only zones with `last_play_state == "playing"` and a non-local output are
/// eligible.
pub fn spawn_auto_resume_listener(state: &AppState) {
    if server_was_down_too_long(state) {
        return;
    }

    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = match zone_repo.list() {
        Ok(z) => z,
        Err(_) => return,
    };

    // Build set of device_ids that need auto-resume
    let mut pending: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for zone in &zones {
        let Some(zone_id) = zone.id else { continue };
        let Some(ref device_id) = zone.output_device_id else {
            continue;
        };
        // Skip local devices (handled by auto_resume_local_zones)
        if device_id.starts_with("local:") {
            continue;
        }
        let play_state = zone_repo.get_last_play_state(zone_id);
        if play_state.as_deref() == Some("playing") {
            pending.insert(device_id.clone(), zone_id);
        }
    }

    if pending.is_empty() {
        debug!("auto_resume_listener_no_eligible_zones");
        return;
    }

    info!(
        count = pending.len(),
        "auto_resume_listener_started_waiting_for_devices"
    );

    let state = state.clone();
    let mut rx = state.event_bus.subscribe();
    tokio::spawn(async move {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(RECONNECT_DEADLINE_SECS);

        // Check devices that already reconnected before we subscribed.
        // SSDP/mDNS discovery may have emitted `device.reconnected` events
        // during boot before this listener was registered, so we do an
        // immediate pass over the output registry.
        {
            let outputs = state.outputs.lock().await;
            let already_online: Vec<String> = pending
                .keys()
                .filter(|id| outputs.contains(id))
                .cloned()
                .collect();
            drop(outputs);
            for device_id in already_online {
                if let Some(zone_id) = pending.remove(&device_id) {
                    info!(
                        zone_id,
                        device_id = %device_id,
                        "auto_resume_device_already_online_attempting"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    try_auto_resume_zone(&state, zone_id).await;
                }
            }
        }
        if pending.is_empty() {
            info!("auto_resume_all_zones_resumed_on_initial_check");
            return;
        }

        loop {
            if pending.is_empty() {
                info!("auto_resume_listener_all_zones_resumed");
                break;
            }

            let recv = tokio::time::timeout_at(deadline, rx.recv());
            match recv.await {
                Ok(Ok(event)) if event.event_type == "device.reconnected" => {
                    let device_id = event.data["device_id"].as_str().unwrap_or("");
                    if let Some(zone_id) = pending.remove(device_id) {
                        info!(
                            zone_id,
                            device_id, "auto_resume_device_reconnected_attempting"
                        );
                        // Small delay to let the output finish registering
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        try_auto_resume_zone(&state, zone_id).await;
                    }
                }
                Ok(Ok(_)) => {
                    // Ignore other events
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                    debug!(skipped = n, "auto_resume_listener_lagged");
                }
                Ok(Err(_)) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Deadline reached
                    if !pending.is_empty() {
                        let remaining: Vec<_> = pending.keys().collect();
                        warn!(
                            remaining = ?remaining,
                            "auto_resume_listener_deadline_reached_some_devices_not_reconnected"
                        );
                    }
                    break;
                }
            }
        }
    });
}
