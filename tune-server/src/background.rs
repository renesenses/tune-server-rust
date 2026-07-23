use std::sync::Arc;

use tracing::{debug, error, info};

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
    spawn_dash_temp_gc();
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
    spawn_community_sync(state);
    spawn_concert_alerts(state);
    spawn_cloud_library_sync(state);
    spawn_local_audio_rescan(state);
    spawn_ssdp_startup_scan(state);
    spawn_slimproto_server(state);
    spawn_social_sharing_listener(state);
    crate::routes::developer_api::spawn_webhook_dispatcher(state);
    #[cfg(feature = "oaat")]
    spawn_oaat_stall_supervisor(state);
    #[cfg(feature = "cloud-relay")]
    spawn_relay_client(state).await;
}

/// Supervise OAAT zones and recover a stalled one with a stop+play restart.
///
/// The OAAT streaming loop can stall when the *source* transcode stream hangs
/// (the transcode downloads the whole track to a temp file before emitting any
/// PCM, so a slow/stalled hi-res download starves `stream.next()`). The output's
/// own 10s watchdog only reconnects the endpoint TCP — the wrong layer — and
/// cannot re-request a transcoded WAV session (they are not seekable and a re-GET
/// re-attaches to the same hung channel). The only reliable recovery is a fresh
/// play, exactly what the `/zones/{id}/stop` + `/zones/{id}/play` sequence does.
///
/// This supervisor polls every 10s, and when an OAAT zone reports a packet stall
/// (`playing && !paused && last_packet_age_ms > 30s`) it re-issues stop+play for
/// the current track. A per-device back-off (≥60s between restarts, give up after
/// 3 consecutive that don't recover → clean stop) prevents a restart loop on a
/// permanently-dead source. History is cleared once a device plays cleanly again.
#[cfg(feature = "oaat")]
fn spawn_oaat_stall_supervisor(state: &AppState) {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    let orchestrator = state.orchestrator.clone();
    let outputs = state.outputs.clone();
    let playback = state.playback.clone();
    let backend = state.backend.clone();

    // Restart only after the stall has persisted well beyond the output's own
    // 10s watchdog window, so a transient hiccup that self-recovers is left alone.
    const STALL_MS: u64 = 30_000;
    const MIN_INTERVAL: Duration = Duration::from_secs(60);
    const MAX_CONSECUTIVE: u32 = 3;

    tokio::spawn(async move {
        // device_id → (last restart, consecutive restart count)
        let mut history: HashMap<String, (Instant, u32)> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(10));

        loop {
            ticker.tick().await;

            // Collect the device_id + current stall state of every OAAT output,
            // then release the registry lock before doing any stop/play I/O.
            let mut states: Vec<(String, bool)> = Vec::new();
            {
                let reg = outputs.lock().await;
                for device_id in reg.list() {
                    if !device_id.starts_with("oaat:") && !device_id.starts_with("oaat-group:") {
                        continue;
                    }
                    if let Some(arc) = reg.get(&device_id) {
                        let out = arc.lock().await;
                        if let Some(oaat) = out
                            .as_any()
                            .downcast_ref::<tune_core::outputs::oaat::OaatOutput>()
                        {
                            let snap = oaat.diagnostics_snapshot();
                            let playing = snap["playing"].as_bool().unwrap_or(false);
                            let paused = snap["paused"].as_bool().unwrap_or(false);
                            let age = snap["last_packet_age_ms"].as_u64().unwrap_or(0);
                            let stalled = playing && !paused && age > STALL_MS;
                            states.push((device_id.clone(), stalled));
                        }
                    }
                }
            }

            for (device_id, stalled) in states {
                if !stalled {
                    // Healthy again → forget any prior restart history so the next
                    // isolated stall starts from a clean consecutive count.
                    history.remove(&device_id);
                    continue;
                }

                let now = Instant::now();
                let count = match history.get(&device_id) {
                    // Backing off: restarted this device too recently, wait.
                    Some((last, _)) if now.duration_since(*last) < MIN_INTERVAL => continue,
                    Some((_, c)) => *c,
                    None => 0,
                };

                let zone = match tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone())
                    .get_by_device_id(&device_id)
                {
                    Ok(Some(z)) => z,
                    _ => continue,
                };
                let zone_id = match zone.id {
                    Some(id) => id,
                    None => continue,
                };

                if count >= MAX_CONSECUTIVE {
                    // Restarts aren't helping — stop cleanly so we stop hammering
                    // the endpoint and leave a well-defined idle state. Once stopped
                    // the zone is no longer "playing" so this won't fire again until
                    // the user replays.
                    error!(
                        zone_id,
                        device_id = %device_id,
                        "oaat_stall_supervisor_giving_up_stopping_zone"
                    );
                    orchestrator.stop(zone_id, Some(&device_id)).await;
                    history.remove(&device_id);
                    continue;
                }

                info!(
                    zone_id,
                    device_id = %device_id,
                    attempt = count + 1,
                    "oaat_stall_supervisor_restarting_zone"
                );

                orchestrator.stop(zone_id, Some(&device_id)).await;
                tokio::time::sleep(Duration::from_secs(2)).await;

                let st = playback.get_state(zone_id).await;
                if let Some(np) = st.now_playing {
                    let req = tune_core::orchestrator::PlayRequest {
                        zone_id,
                        output_device_id: Some(device_id.clone()),
                        track_id: np.track_id,
                        source: if np.source == "local" {
                            None
                        } else {
                            Some(np.source.clone())
                        },
                        source_id: np.source_id.clone(),
                        title: Some(np.title.clone()),
                        artist_name: np.artist_name.clone(),
                        album_title: np.album_title.clone(),
                        cover_url: np.cover_path.clone(),
                        duration_ms: Some(np.duration_ms),
                        seek_ms: None,
                        temp_file_path: None,
                        sample_rate: None,
                        bit_depth: None,
                        media_format: None,
                    };
                    match orchestrator.play(req).await {
                        Ok(_) => {
                            // Restore the queue length from the DB so the poller
                            // keeps auto-advancing after the restart (mirrors the
                            // /zones/{id}/play handler; without it a mid-album stall
                            // recovery would play the current track then stop).
                            let qr = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(
                                backend.clone(),
                            );
                            let q_len = qr.count(zone_id).unwrap_or(0)
                                + qr.count_streaming(zone_id).unwrap_or(0);
                            if q_len > 0 {
                                let cur_pos = playback.get_state(zone_id).await.queue_position;
                                playback.update_queue_info(zone_id, cur_pos, q_len).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(zone_id, error = %e, "oaat_stall_supervisor_replay_failed");
                        }
                    }
                }

                history.insert(device_id.clone(), (now, count + 1));
            }
        }
    });
}

#[cfg(feature = "cloud-relay")]
async fn spawn_relay_client(state: &AppState) {
    // Premium gate: Cloud Relay requires Premium
    if !state
        .license
        .check_feature(tune_core::license::Feature::CloudRelay)
        .await
    {
        info!("cloud_relay_requires_premium");
        return;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if let Some(_client) = tune_core::cloud::relay::spawn_relay_client(&settings, state.port) {
        info!("cloud relay client spawned");
    }
}

fn spawn_ssdp_startup_scan(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // Multiple scan passes to catch slow DLNA renderers (DMP-A8, etc.)
        // that don't respond to the first SSDP multicast.
        for (pass, delay_secs) in [(1, 3), (2, 8), (3, 15)] {
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            if pass == 1 {
                info!("ssdp_startup_scan_starting");
            }

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
                            let cm_url = service_urls
                                .get("connectionmanager")
                                .or_else(|| service_urls.get("ConnectionManager"))
                                .map(|p| format!("{base}{p}"));
                            let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                                d.name.clone(),
                                d.id.clone(),
                                d.host.clone(),
                                format!("{base}{av}"),
                                format!("{base}{rc}"),
                                cm_url,
                            );
                            outputs.register(Box::new(dlna));
                            registered += 1;
                        }
                    }
                }
            }
            drop(outputs);

            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            for d in &devices {
                // Respect user deletions: a device the user removed is marked
                // hidden. The mDNS/SSDP live handlers already skip zone creation
                // for hidden devices — this startup batch path must do the same,
                // otherwise a deleted zone silently reappears on every restart
                // (Fabien: "Salon: AIRPLAY" zone came back after update).
                if zone_repo.is_device_hidden(&d.id) {
                    info!(name = %d.name, device_id = %d.id, "ssdp_startup_zone_hidden_skipping");
                    continue;
                }

                // Auto-created zones start dormant and don't count against the
                // free tier; the cap is enforced at first play in
                // orchestrator.play(). So discovery may always register a device.
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
                pass,
                "ssdp_startup_scan_complete"
            );

            if pass > 1 && registered == 0 {
                break;
            }
        }
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

/// Sweep leaked Tidal/Qobuz DASH temp files (`tune-dash-*.mp4`). The local
/// transcode no longer deletes the tidal-cache-owned source right after decoding
/// (that caused the ASIO repeat re-download runaway), so files older than well
/// past the cache TTL — no longer served from cache nor being decoded — are
/// removed here to avoid accumulating ~54MB files across a listening session.
fn spawn_dash_temp_gc() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            ticker.tick().await;
            let dir = std::env::temp_dir();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let now = std::time::SystemTime::now();
            let mut removed = 0u32;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with("tune-dash-") && name.ends_with(".mp4")) {
                    continue;
                }
                let too_old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .map(|age| age.as_secs() > 600) // 10 min ≫ 240s cache TTL
                    .unwrap_or(false);
                if too_old && std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
            if removed > 0 {
                info!(removed, "dash_temp_gc_sweep");
            }
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
/// can see all running instances in real-time.  Also carries license_key
/// and hardware_fingerprint so the server can validate the license and
/// return tier / expiry information.
fn spawn_heartbeat(state: &AppState) {
    let backend = state.backend.clone();
    let services = state.services.clone();
    let outputs = state.outputs.clone();
    let started_at = state.started_at;
    let license = state.license.clone();
    let event_bus = state.event_bus.clone();
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

        let client = match tune_core::http::client::builder()
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
            // Use try_lock to avoid blocking the heartbeat if another
            // task holds the services or outputs lock.
            let authenticated_services: Vec<String> = match services.try_lock() {
                Ok(registry) => {
                    let names = registry.list();
                    let svc_handles: Vec<_> = names
                        .iter()
                        .filter_map(|n| registry.get(n).map(|h| (n.clone(), h)))
                        .collect();
                    drop(registry);

                    let mut authed = Vec::new();
                    for (name, handle) in svc_handles {
                        if let Ok(svc) = handle.try_lock() {
                            let status = svc.auth_status().await;
                            if status.authenticated {
                                authed.push(name);
                            }
                        }
                    }
                    authed
                }
                Err(_) => Vec::new(),
            };

            // Look up friendly names from zones DB
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone());
            let zone_names: std::collections::HashMap<String, String> = zone_repo
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|z| z.output_device_id.map(|did| (did, z.name)))
                .collect();

            let devices: Vec<serde_json::Value> = match outputs.try_lock() {
                Ok(registry) => registry
                    .list()
                    .into_iter()
                    .map(|id| {
                        let dev_type = if id.starts_with("local:") {
                            "local"
                        } else if id.starts_with("airplay-") {
                            "airplay"
                        } else if id.starts_with("chromecast-") {
                            "chromecast"
                        } else if id.starts_with("oaat:") {
                            "oaat"
                        } else if id.starts_with("uuid:") {
                            "dlna"
                        } else {
                            "other"
                        };
                        let name = zone_names.get(&id).map(|n| n.as_str()).unwrap_or_else(|| {
                            id.strip_prefix("local:")
                                .or_else(|| id.strip_prefix("uuid:"))
                                .unwrap_or(&id)
                        });
                        serde_json::json!({ "name": name, "type": dev_type })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };

            // Include license info so the server can validate and respond
            // with the authoritative tier / expiry.
            let ls = license.license_state().await;

            let payload = serde_json::json!({
                "instance_id": instance_id,
                "version": tune_core::version(),
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "tracks": tracks,
                "uptime_s": uptime_s,
                "hostname": hostname,
                "services": authenticated_services,
                "devices": devices,
                "license_key": ls.license_key,
                "hardware_fingerprint": ls.hardware_fingerprint,
            });

            // Update server_last_alive_at timestamp for crash detection
            {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
                let now_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                settings
                    .set("server_last_alive_at", &now_ts.to_string())
                    .ok();
            }

            match client
                .post("https://mozaiklabs.fr/api/v1/heartbeat")
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    debug!(instance_id = %instance_id, tracks, uptime_s, "heartbeat_sent");

                    // Parse license validation data from the response body.
                    // The server may or may not include license fields — if
                    // absent (old server, 204, empty body, etc.) we keep the
                    // cached state unchanged.
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(tier_str) = body.get("license_tier").and_then(|v| v.as_str()) {
                            let valid = body
                                .get("license_valid")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true);

                            if !valid {
                                // Server explicitly says the license is invalid.
                                info!("license_invalidated_by_server");
                                license
                                    .update_from_server(tune_core::license::Tier::Free, None)
                                    .await;
                                event_bus.emit(
                                    "license.updated",
                                    serde_json::json!({
                                        "tier": "free",
                                        "expires_at": null,
                                    }),
                                );
                            } else {
                                let tier = match tier_str {
                                    "premium" => tune_core::license::Tier::Premium,
                                    _ => tune_core::license::Tier::Free,
                                };
                                let expires_at = body
                                    .get("license_expires_at")
                                    .and_then(|v| v.as_str())
                                    .map(String::from);

                                license.update_from_server(tier, expires_at.clone()).await;
                                info!(tier = %tier, "license_validated_from_heartbeat");
                                event_bus.emit(
                                    "license.updated",
                                    serde_json::json!({
                                        "tier": tier,
                                        "expires_at": expires_at,
                                    }),
                                );
                            }
                        }
                        // else: no license fields in response — keep cached state.
                    }
                }
                Ok(resp) => {
                    debug!(status = %resp.status(), "heartbeat_rejected");
                }
                Err(e) => {
                    debug!(error = %e, "heartbeat_failed");
                }
            }

            // Refresh the account premium (SSO) from /api/v1/user so a lapsed
            // subscription is picked up without waiting for the offline grace or
            // a re-login. No-op when not connected. Never blocks the heartbeat.
            refresh_account_premium(&backend, &license).await;

            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}

/// Re-fetch the mozaiklabs.fr account profile and update the account premium
/// (SSO). No-op if not connected (no access token). On an expired access token,
/// tries the refresh_token grant once and retries. On any network failure the
/// cached state is kept (the offline grace in `LicenseManager` covers it).
async fn refresh_account_premium(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    license: &Arc<tune_core::license::LicenseManager>,
) {
    use tune_core::cloud::sso::{DEFAULT_CLIENT_ID, MozaikAuth};

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());

    let token = match settings.get("mozaik_access_token").ok().flatten() {
        Some(t) if !t.is_empty() => t,
        _ => return, // not connected — nothing to refresh
    };

    let client_id = settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_MOZAIK_CLIENT_ID").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    if client_id.is_empty() {
        return;
    }
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let auth = MozaikAuth::new(client_id, base_url.as_deref());

    // Try the current token; if it fails (likely expired), refresh once & retry.
    let user = match auth.get_user(&token).await {
        Ok(u) => Some(u),
        Err(_) => {
            let refresh = settings
                .get("mozaik_refresh_token")
                .ok()
                .flatten()
                .filter(|s| !s.is_empty());
            match refresh {
                Some(rt) => match auth.refresh_token(&rt).await {
                    Ok(tok) => {
                        settings.set("mozaik_access_token", &tok.access_token).ok();
                        if let Some(ref new_rt) = tok.refresh_token {
                            settings.set("mozaik_refresh_token", new_rt).ok();
                        }
                        auth.get_user(&tok.access_token).await.ok()
                    }
                    Err(e) => {
                        debug!(error = %e, "mozaik_token_refresh_failed");
                        None
                    }
                },
                None => None,
            }
        }
    };

    if let Some(user) = user {
        settings
            .set(
                "mozaik_user",
                &serde_json::to_string(&user).unwrap_or_default(),
            )
            .ok();
        license
            .set_account_premium(user.premium, user.license_expires_at.clone())
            .await;
        debug!(premium = user.premium, "mozaik_account_premium_refreshed");
    }
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

fn spawn_slimproto_server(state: &AppState) {
    let local_ip = tune_core::discovery::ssdp::get_local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // Wire the server to app state so connected players register as zones and
    // can be driven for playback.
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let outputs = state.outputs.clone();
    let server_ip = local_ip.clone();
    tokio::spawn(async move {
        let server = Arc::new(tune_core::slimproto::SlimProtoServer::new_with_state(
            db, event_bus, outputs, server_ip,
        ));
        if let Err(e) = server.spawn().await {
            error!(error = %e, "slimproto_server_failed");
        }
    });
    let cli_state = Arc::new(tune_core::slimproto::cli_server::CliState {
        players: tune_core::slimproto::new_player_registry(),
        server_name: "Tune".to_string(),
        server_version: tune_core::version().to_string(),
        local_ip,
    });
    tokio::spawn(tune_core::slimproto::cli_server::start_cli_server(
        cli_state,
    ));
}

fn spawn_bio_sync(state: &AppState) {
    let license = state.license.clone();
    let db = state.backend.clone();
    let rx = state.event_bus.subscribe();
    tokio::spawn(async move {
        // Wait for startup to settle before checking license
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if !license
            .check_feature(tune_core::license::Feature::AutoEnrichment)
            .await
        {
            info!("bio_sync_auto_download_requires_premium — upload-only mode");
            // Still upload local bios (community contribution) but skip auto download
            let db_upload = db.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(55)).await;
                loop {
                    tune_core::cloud::bio_sync::upload_bios(&db_upload).await;
                    tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                }
            });
            return;
        }
        tune_core::cloud::bio_sync::spawn(db, rx);
    });
}

fn spawn_community_sync(state: &AppState) {
    tune_core::cloud::community_sync::spawn(state.backend.clone());
}

fn spawn_concert_alerts(state: &AppState) {
    tune_core::cloud::concert_alerts::spawn(state.backend.clone());
}

fn spawn_cloud_library_sync(state: &AppState) {
    tune_core::cloud::library_sync::spawn(state.backend.clone(), state.license.clone());
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

/// Whether any registered local (`local:`) output is currently playing. Used to
/// suppress device enumeration (which probes formats and can crash the active
/// WASAPI stream on Windows — DEvir) while local playback is in progress.
#[cfg(feature = "local-audio")]
pub async fn any_local_output_playing(state: &AppState) -> bool {
    let outputs = state.outputs.lock().await;
    for id in outputs.list() {
        if !id.starts_with("local:") {
            continue;
        }
        if let Some(output) = outputs.get(&id) {
            let output = output.lock().await;
            if let Ok(status) = output.get_status().await {
                if status.state == tune_core::outputs::traits::TransportState::Playing {
                    return true;
                }
            }
        }
    }
    false
}

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
    let configured_backend = {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        settings
            .get("local_audio_backend")
            .ok()
            .flatten()
            .unwrap_or_else(|| state.config.local_audio_backend.clone())
    };
    // When ASIO is configured, force WASAPI for the periodic rescan
    // (re-probing ASIO during playback can crash the driver).
    // ASIO devices were registered at startup and won't change.
    let scan_backend = if configured_backend.eq_ignore_ascii_case("asio") {
        "wasapi".to_string()
    } else {
        configured_backend.clone()
    };
    let is_asio_configured = configured_backend.eq_ignore_ascii_case("asio");

    // Do NOT re-enumerate audio devices while a local output is actively
    // playing. On Windows the WASAPI enumeration probes each device's supported
    // formats, which can invalidate the active render stream and kill playback:
    // refreshing the UI during local playback triggered a hotplug rescan whose
    // enumeration crashed the active fallback stream (audio_stream_error: "The
    // requested device is no longer available") → 10s decoder timeout → total
    // stop (DEvir, Win11 WASAPI fallback). Hotplug detection resumes on the next
    // cycle once playback stops. This also protects any active ASIO output.
    if any_local_output_playing(state).await {
        debug!("local_audio_rescan_skipped_active_playback");
        return;
    }

    let backend_clone = scan_backend.clone();
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

            // Already registered — still ensure a zone exists (may have been deleted)
            if existing_ids.contains(&device_id) || outputs.contains(&device_id) {
                new_devices_to_zone.push((device_id, dev.name.clone(), dev.is_default));
                continue;
            }

            // New device found — register it
            let local_out = tune_core::outputs::local::LocalOutput::with_options(
                dev.name.clone(),
                state.config.local_exclusive_mode,
                &configured_backend,
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
            // When ASIO is configured, don't create new zones for WASAPI
            // devices discovered by the fallback rescan. Users should only
            // see ASIO zones (e.g. "HoloAudio ASIO Driver"), not confusing
            // generic WASAPI names like "Haut-parleurs" for the same DAC.
            // Only update online status for WASAPI zones that already exist.
            if is_asio_configured {
                continue;
            }
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
                Ok((zid, false)) => {
                    let _ = zone_repo.set_online_by_device(device_id, true);
                    debug!(zone_id = zid, device_id = %device_id, "local_audio_zone_set_online");
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

fn spawn_social_sharing_listener(state: &AppState) {
    let license = state.license.clone();
    let backend = state.backend.clone();
    let mut rx = state.playback.subscribe();
    let http_client = state.http_client.clone();

    tokio::spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!(skipped = n, "social_sharing_listener_lagged");
                    continue;
                }
                Err(_) => break,
            };

            // Only react to track-start events
            if event.event != "started" {
                continue;
            }

            // Premium gate
            if !license
                .check_feature(tune_core::license::Feature::SocialSharing)
                .await
            {
                continue;
            }

            // Check sharing profile
            let profile = tune_core::social::load_profile(&backend);
            if !profile.enabled || !profile.share_now_playing {
                continue;
            }

            // Build the card from event data
            let title = event
                .data
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let artist = event
                .data
                .get("artist_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let album = event
                .data
                .get("album_title")
                .and_then(|v| v.as_str())
                .map(String::from);
            let cover = event
                .data
                .get("cover_path")
                .and_then(|v| v.as_str())
                .map(String::from);
            let source = event
                .data
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("local")
                .to_string();

            if title.is_empty() {
                continue;
            }

            let card = tune_core::social::NowListeningCard {
                title,
                artist,
                album,
                cover_url: cover,
                format: None,
                sample_rate: None,
                bit_depth: None,
                source,
                shared_at: time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            };

            let payload = serde_json::json!({
                "display_name": profile.display_name,
                "now_listening": card,
            });

            let client = http_client.clone();
            tokio::spawn(async move {
                match client
                    .post("https://mozaiklabs.fr/api/v1/community/now-listening")
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        debug!("social_auto_share_ok");
                    }
                    Ok(resp) => {
                        debug!(
                            status = resp.status().as_u16(),
                            "social_auto_share_upstream_error"
                        );
                    }
                    Err(e) => {
                        debug!(error = %e, "social_auto_share_failed");
                    }
                }
            });
        }
    });
}
