use std::sync::Arc;

use tracing::{debug, info};

use tune_core::outputs::OutputRegistry;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Spawn all periodic background tasks: squeezebox polling, session GC, position poller,
/// token refresh, UPnP advertiser, alarm scheduler, Deezer proxy config, desktop notifications,
/// and RSS memory diagnostics.
pub async fn spawn_background_tasks(state: &AppState, config: &TuneConfig) {
    spawn_squeezebox_poller(state);
    spawn_hqplayer_poller(state);
    spawn_session_gc(state);
    spawn_position_poller(state);
    spawn_token_refresher(state);
    spawn_upnp_advertiser(state, config).await;
    configure_deezer_proxy(state, config).await;
    spawn_alarm_scheduler(state);
    spawn_desktop_notifications(state, config);
    spawn_memory_diagnostics(state.outputs.clone());
    spawn_telemetry_reporter(state);
    spawn_heartbeat(state);
    spawn_bio_sync(state);
    spawn_local_audio_rescan(state);
    spawn_ssdp_startup_scan(state);
}

fn spawn_ssdp_startup_scan(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // Wait for the network stack and mDNS to settle before scanning.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        info!("ssdp_startup_scan_starting");

        let scanner = state.scanner.lock().await;
        let devices = scanner.rescan().await;
        drop(scanner);

        let mut registered = 0u32;
        let mut outputs = state.outputs.lock().await;
        for d in &devices {
            let location = d.location.as_deref().unwrap_or("");
            if location.is_empty() || outputs.contains(&d.id) {
                continue;
            }
            if let Ok(desc) =
                tune_core::discovery::xml_parser::fetch_device_description(location).await
            {
                if desc.is_media_renderer() {
                    let service_urls = desc.service_urls();
                    if let (Some(av), Some(rc)) = (
                        service_urls.get("avtransport"),
                        service_urls.get("renderingcontrol"),
                    ) {
                        let base = format!("http://{}:{}", d.host, d.port);
                        let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                            d.name.clone(),
                            d.id.clone(),
                            d.host.clone(),
                            format!("{base}{av}"),
                            format!("{base}{rc}"),
                        );
                        outputs.register(Box::new(dlna));
                        registered += 1;
                    }
                }
            }
        }
        drop(outputs);

        // Auto-create zones for discovered devices (skip if zone already exists)
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        for d in &devices {
            match zone_repo.get_or_create(&d.name, Some("dlna"), &d.id) {
                Ok((zid, true)) => {
                    info!(name = %d.name, zone_id = zid, device_id = %d.id, "ssdp_startup_zone_created");
                }
                Ok((_, false)) => {
                    let _ = zone_repo.set_online_by_device(&d.id, true);
                }
                Err(e) => {
                    tracing::warn!(name = %d.name, device_id = %d.id, error = %e, "ssdp_startup_zone_create_failed");
                }
            }
        }

        info!(
            registered,
            total = devices.len(),
            "ssdp_startup_scan_complete"
        );
    });
}

fn spawn_squeezebox_poller(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            let enabled = settings
                .get("squeezebox_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            let host = settings
                .get("lms_host")
                .ok()
                .flatten()
                .or_else(|| settings.get("squeezebox_host").ok().flatten())
                .unwrap_or_default();

            if enabled && !host.is_empty() {
                match crate::routes::squeezebox::discover_and_register(&state).await {
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

            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

fn spawn_hqplayer_poller(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        loop {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            let enabled = settings
                .get("hqplayer_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            let host = settings
                .get("hqplayer_host")
                .ok()
                .flatten()
                .unwrap_or_default();

            if enabled && !host.is_empty() {
                match crate::routes::hqplayer::discover_and_register(&state).await {
                    Ok(_) => {
                        info!(host = %host, "hqplayer_poll_registered");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, host = %host, "hqplayer_poll_failed");
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

fn spawn_session_gc(state: &AppState) {
    let streamer = state.streamer.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let removed = streamer.cleanup_stale_sessions().await;
            if removed > 0 {
                info!(removed, "session_gc_sweep");
            }
        }
    });
}

fn spawn_position_poller(state: &AppState) {
    let poller = tune_core::poller::PositionPoller::new(
        state.orchestrator.clone(),
        state.playback.clone(),
        state.outputs.clone(),
        state.backend.clone(),
        state.poller_metrics.clone(),
    );
    poller.spawn();
}

fn spawn_token_refresher(state: &AppState) {
    let services = state.services.clone();
    let db = state.backend.clone();
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
                                    tune_core::db::settings_repo::SettingsRepo::with_backend(
                                        db.clone(),
                                    );
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

async fn spawn_upnp_advertiser(state: &AppState, config: &TuneConfig) {
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
}

async fn configure_deezer_proxy(state: &AppState, config: &TuneConfig) {
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

fn spawn_alarm_scheduler(state: &AppState) {
    let alarm_sched = Arc::new(tune_core::alarms::AlarmScheduler::with_backend(
        state.backend.clone(),
        state.orchestrator.clone(),
    ));
    alarm_sched.spawn();
}

fn spawn_desktop_notifications(state: &AppState, config: &TuneConfig) {
    if tune_core::notifications::is_enabled() {
        let server_ip = tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let server_base = Arc::new(format!("http://{}:{}", server_ip, config.port));
        tune_core::notifications::spawn_notification_listener(
            state.event_bus.subscribe(),
            server_base,
        );
    }
}

fn spawn_telemetry_reporter(state: &AppState) {
    tune_core::cloud::telemetry::spawn_startup_ping(state.services.clone());
    tune_core::cloud::telemetry::TelemetryReporter::spawn(
        state.backend.clone(),
        state.services.clone(),
    );
}

/// Lightweight heartbeat — runs ALWAYS regardless of TUNE_TELEMETRY.
/// Sends a minimal ping every 5 minutes to mozaiklabs.fr so the admin
/// can see all running instances in real-time.
fn spawn_heartbeat(state: &AppState) {
    let backend = state.backend.clone();
    let services = state.services.clone();
    let started_at = state.started_at;
    tokio::spawn(async move {
        // Let startup finish before the first heartbeat
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        let instance_id = match settings.get("instance_id").ok().flatten() {
            Some(id) if !id.is_empty() => id,
            _ => {
                let id = uuid::Uuid::new_v4().to_string();
                settings.set("instance_id", &id).ok();
                id
            }
        };

        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| gethostname().unwrap_or_else(|| "unknown".into()));

        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "heartbeat_client_build_failed");
                return;
            }
        };

        loop {
            let tracks = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
                .count()
                .unwrap_or(0);
            let uptime_s = started_at.elapsed().as_secs();

            // Collect authenticated streaming services
            let authenticated_services: Vec<String> = {
                let registry = services.lock().await;
                let names = registry.list();
                let svc_handles: Vec<_> = names
                    .iter()
                    .filter_map(|n| registry.get(n).map(|h| (n.clone(), h)))
                    .collect();
                drop(registry); // release registry lock before checking auth

                let mut authed = Vec::new();
                for (name, handle) in svc_handles {
                    let svc = handle.lock().await;
                    let status = svc.auth_status().await;
                    if status.authenticated {
                        authed.push(name);
                    }
                }
                authed
            };

            let payload = serde_json::json!({
                "instance_id": instance_id,
                "version": tune_core::version(),
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "tracks": tracks,
                "uptime_s": uptime_s,
                "hostname": hostname,
                "services": authenticated_services,
            });

            match client
                .post("https://mozaiklabs.fr/api/v1/heartbeat")
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    debug!(instance_id = %instance_id, tracks, uptime_s, "heartbeat_sent");
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "heartbeat_rejected");
                }
                Err(e) => {
                    debug!(error = %e, "heartbeat_failed");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}

/// Resolve the machine hostname via the `hostname` command.
fn gethostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

fn spawn_bio_sync(state: &AppState) {
    tune_core::cloud::bio_sync::spawn(state.backend.clone(), state.event_bus.subscribe());
}

fn spawn_memory_diagnostics(outputs: Arc<tokio::sync::Mutex<OutputRegistry>>) {
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
            let _ = &outputs; // keep alive on non-linux
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}

/// Periodically re-enumerate local audio devices (every 120s) to detect USB DACs
/// that were plugged in after startup or took time to initialize.
#[cfg(feature = "local-audio")]
fn spawn_local_audio_rescan(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // Initial delay to avoid conflicting with startup registration
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            rescan_local_audio_devices(&state).await;
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        }
    });
}

#[cfg(not(feature = "local-audio"))]
fn spawn_local_audio_rescan(_state: &AppState) {}

/// Re-enumerate local audio devices and register any new ones.
/// Removes devices that have disappeared (unless actively playing).
#[cfg(feature = "local-audio")]
pub async fn rescan_local_audio_devices(state: &AppState) {
    // On Windows, use WASAPI for the periodic rescan instead of re-probing
    // ASIO every cycle.  Re-probing ASIO can crash the process when the ASIO
    // driver is in a bad state (e.g. SOtM Diretta via RDP — the ASIO SDK
    // calls abort() internally, killing the process with no panic/error).
    // ASIO devices are detected at startup; the hotplug rescan only needs to
    // track WASAPI device changes (USB DACs plugged/unplugged).
    #[cfg(target_os = "windows")]
    let audio_backend = "wasapi".to_string();
    #[cfg(not(target_os = "windows"))]
    let audio_backend = state.config.local_audio_backend.clone();
    let backend_clone = audio_backend.clone();
    let devices = match tokio::task::spawn_blocking(move || {
        tune_core::outputs::local::list_audio_devices_with_backend(&backend_clone)
    })
    .await
    {
        Ok(d) => d,
        Err(_) => return,
    };

    // Collect new device IDs first (no lock needed)
    let new_device_ids: std::collections::HashSet<String> = devices
        .iter()
        .map(|dev| format!("local:{}", dev.name))
        .collect();

    // Phase 1: Register new devices and remove stale ones (hold lock briefly)
    let mut new_devices_to_zone: Vec<(String, String, bool)> = Vec::new();
    {
        let mut outputs = state.outputs.lock().await;
        let existing_ids: std::collections::HashSet<String> = outputs
            .list()
            .into_iter()
            .filter(|id| id.starts_with("local:"))
            .collect();

        let mut registered_count = 0;

        for dev in &devices {
            let device_id = format!("local:{}", dev.name);

            // Skip if already registered
            if existing_ids.contains(&device_id) || outputs.contains(&device_id) {
                continue;
            }

            // New device found — register it
            let local_out = tune_core::outputs::local::LocalOutput::with_options(
                dev.name.clone(),
                state.config.local_exclusive_mode,
                &audio_backend,
            );
            outputs.register(Box::new(local_out));
            registered_count += 1;

            info!(
                name = %dev.name,
                device_id = %device_id,
                default = dev.is_default,
                channels = dev.max_channels,
                "local_audio_hotplug_detected"
            );

            new_devices_to_zone.push((device_id, dev.name.clone(), dev.is_default));
        }

        // Remove WASAPI devices that have disappeared (USB DAC unplugged),
        // but only if not actively playing.
        // On Windows, only remove devices that were found in the current
        // scan backend (WASAPI). Devices registered by ASIO at startup
        // won't appear in WASAPI scans — don't remove them, as dropping
        // ASIO outputs can crash the process via the driver FFI.
        for old_id in &existing_ids {
            if new_device_ids.contains(old_id) {
                continue;
            }

            // Only remove if the device name matches one we could have
            // discovered with the current scan backend.  If the scan used
            // WASAPI but this device was registered by ASIO at startup,
            // it won't be in new_device_ids but we must NOT remove it.
            // If the scan returned nothing, skip all removals — an empty
            // result means the backend couldn't enumerate (e.g. WASAPI held
            // exclusively by foobar2000), not that everything disappeared.
            if devices.is_empty() {
                debug!("local_audio_rescan_empty_skipping_all_removals");
                break;
            }
            let old_name = old_id.strip_prefix("local:").unwrap_or(old_id);
            let was_in_scan_scope = devices.iter().any(|d| d.name == old_name);
            if !was_in_scan_scope {
                debug!(device_id = %old_id, "local_audio_skipping_removal_different_backend");
                continue;
            }

            let is_playing = if let Some(output) = outputs.get(old_id) {
                let output = output.lock().await;
                match output.get_status().await {
                    Ok(status) => {
                        status.state == tune_core::outputs::traits::TransportState::Playing
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !is_playing {
                outputs.remove(old_id);
                info!(device_id = %old_id, "local_audio_device_removed");
            }
        }

        if registered_count > 0 {
            info!(
                new_devices = registered_count,
                total = devices.len(),
                "local_audio_rescan_complete"
            );
        }
    } // outputs lock released here

    // Phase 2: Create zones and emit events (no lock held)
    if !new_devices_to_zone.is_empty() {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());

        for (device_id, dev_name, is_default) in &new_devices_to_zone {
            let zone_name = if *is_default {
                "This Computer".to_string()
            } else {
                dev_name.clone()
            };
            match zone_repo.get_or_create(&zone_name, Some("local"), device_id) {
                Ok((zid, true)) => {
                    info!(
                        name = %zone_name,
                        zone_id = zid,
                        device_id = %device_id,
                        "local_audio_hotplug_zone_created"
                    );
                }
                Ok((_, false)) => {
                    // Zone already exists — nothing to do
                }
                Err(e) => {
                    tracing::warn!(name = %zone_name, device_id = %device_id, error = %e, "local_audio_hotplug_zone_create_failed");
                }
            }

            // Emit event for UI refresh
            state.event_bus.emit(
                "device.discovered",
                serde_json::json!({
                    "id": device_id,
                    "name": dev_name,
                    "type": "local",
                    "hotplug": true,
                }),
            );
        }
    }
}
