use std::sync::Arc;

use tracing::info;

use tune_core::db::backend::DbBackend;
use tune_core::outputs::OutputRegistry;
use tune_core::outputs::oh_events::OpenHomeEventListener;

use tune_core::event_bus::EventBus;
use tune_core::event_types::EventType;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Set a zone's online state and, if it actually changed, broadcast a
/// `zone.updated` event so controllers see availability flip in real time.
/// (`set_online_by_device` alone is silent — clients never learned of it.)
fn set_zone_online(event_bus: &EventBus, db: &Arc<dyn DbBackend>, device_id: &str, online: bool) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let prev = zone_repo
        .get_by_device_id(device_id)
        .ok()
        .flatten()
        .map(|z| z.online);
    let _ = zone_repo.set_online_by_device(device_id, online);
    if prev != Some(online) {
        event_bus.emit_typed(
            EventType::ZoneUpdated,
            serde_json::json!({ "device_id": device_id, "online": online }),
        );
    }
}

/// Spawn the SSDP handler that registers DLNA/OpenHome outputs and auto-creates zones.
pub fn spawn_ssdp_handler(
    state: &AppState,
    config: &TuneConfig,
    oh_listener: Option<Arc<OpenHomeEventListener>>,
) {
    let (ssdp_tx, mut ssdp_rx) = tokio::sync::mpsc::channel(64);
    {
        let scanner = state.scanner.clone();
        tokio::spawn(async move {
            let mut scanner = scanner.lock().await;
            *scanner = tune_core::discovery::ssdp::SsdpScanner::new(ssdp_tx);
            scanner.start().await;
        });
    }

    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let config = config.clone();
    let event_bus = state.event_bus.clone();
    let media_servers = state.media_servers.clone();
    let playback = state.playback.clone();
    let license = state.license.clone();
    tokio::spawn(async move {
        use tune_core::discovery::ssdp::SsdpEvent;
        while let Some(event) = ssdp_rx.recv().await {
            match event {
                SsdpEvent::DeviceDiscovered(dev) => {
                    handle_ssdp_discovered(
                        &dev,
                        &outputs,
                        &db,
                        &config,
                        &event_bus,
                        &oh_listener,
                        &playback,
                        &license,
                    )
                    .await;
                }
                SsdpEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    reg.remove(&id);
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus
                        .emit_typed(EventType::DeviceLost, serde_json::json!({ "device_id": id }));
                    info!(id = %id, "output_removed_zone_offline");
                }
                SsdpEvent::MediaServerDiscovered(ms) => {
                    let id = ms.id.clone();
                    media_servers.lock().await.insert(id.clone(), ms);
                    info!(id = %id, "media_server_registered");
                }
            }
        }
    });
}

async fn handle_ssdp_discovered(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    outputs: &Arc<tokio::sync::Mutex<OutputRegistry>>,
    db: &Arc<dyn DbBackend>,
    config: &TuneConfig,
    event_bus: &Arc<tune_core::event_bus::EventBus>,
    oh_listener: &Option<Arc<OpenHomeEventListener>>,
    playback: &Arc<tune_core::playback::PlaybackManager>,
    license: &Arc<tune_core::license::LicenseManager>,
) {
    let is_renderer = dev.device_type == tune_core::discovery::device::OutputType::Dlna
        || dev.device_type == tune_core::discovery::device::OutputType::Openhome;
    if !is_renderer {
        return;
    }

    let svc_urls = dev
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();

    if dev.device_type == tune_core::discovery::device::OutputType::Openhome {
        let evt_urls = dev
            .capabilities
            .get("event_sub_urls")
            .and_then(|v| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
            })
            .unwrap_or_default();
        let oh = tune_core::outputs::openhome::OpenHomeOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            dev.port,
            svc_urls.clone(),
            oh_listener.clone(),
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
        let cm_url = svc_urls
            .get("connectionmanager")
            .or_else(|| svc_urls.get("ConnectionManager"))
            .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
        if let (Some(av), Some(rc)) = (av_url, rc_url) {
            let delay = config.play_delay_for(&dev.name);
            let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                dev.name.clone(),
                dev.id.clone(),
                dev.host.clone(),
                av,
                rc,
                cm_url,
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

    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
        set_zone_online(event_bus, db, &dev.id, true);
        if let Some(zone_id) = zone.id {
            let vol = zone.volume as f64 / 100.0;
            playback.set_volume(zone_id, vol).await;
        }
        info!(name = %dev.name, id = %dev.id, "zone_device_reconnected");
        event_bus.emit(
            "device.reconnected",
            serde_json::json!({
                "device_id": &dev.id,
                "name": &dev.name,
                "host": &dev.host,
            }),
        );
    } else if !is_tv {
        // Premium gate: check zone limit before auto-creating
        let zone_count = zone_repo.count_online().unwrap_or(0);
        if !license.check_zone_limit(zone_count).await {
            info!(
                name = %dev.name,
                zone_count,
                "ssdp_zone_creation_blocked_free_tier_limit"
            );
            return;
        }

        let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
        let existing = zone_repo.list().unwrap_or_default();
        let name_taken = existing.iter().any(|z| z.name == short_name);
        let zone_name = if name_taken {
            dev.name.clone()
        } else {
            short_name.to_string()
        };
        let type_str = if dev.device_type == tune_core::discovery::device::OutputType::Openhome {
            "openhome"
        } else {
            "dlna"
        };
        match zone_repo.get_or_create(&zone_name, Some(type_str), &dev.id) {
            Ok((zid, true)) => {
                event_bus.emit_typed(
                    EventType::ZoneCreated,
                    serde_json::json!({
                        "zone_id": zid,
                        "name": zone_name,
                        "device_id": dev.id,
                        "type": type_str,
                    }),
                );
                info!(name = %zone_name, zone_id = zid, device = %dev.id, r#type = type_str, "ssdp_zone_auto_created");
            }
            Ok((zid, false)) => {
                set_zone_online(event_bus, db, &dev.id, true);
                info!(name = %zone_name, zone_id = zid, device = %dev.id, "ssdp_zone_already_existed");
            }
            Err(e) => {
                tracing::warn!(name = %zone_name, device = %dev.id, error = %e, "ssdp_zone_create_failed");
            }
        }
    }
}

/// Spawn the mDNS handler that registers Chromecast/AirPlay/BluOS/OAAT/Squeezebox outputs.
///
/// Returns the `MdnsScanner` handle (must be kept alive for the scanner to keep running).
pub fn spawn_mdns_handler(state: &AppState) -> Option<tune_core::discovery::mdns::MdnsScanner> {
    let (mdns_tx, mut mdns_rx) = tokio::sync::mpsc::channel(64);
    let handle = if let Ok(mdns) = tune_core::discovery::mdns::MdnsScanner::new(mdns_tx) {
        let mut mdns = mdns
            .with_chromecast()
            .with_airplay()
            .with_bluos()
            .with_oaat()
            .with_squeezebox();
        if let Err(e) = mdns.start() {
            tracing::warn!(error = %e, "mdns_start_failed");
        }
        let port = std::env::var("TUNE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8888u16);
        if let Err(e) = mdns.register_self(port, tune_core::version()) {
            tracing::warn!(error = %e, "mdns_register_self_failed");
        }
        Some(mdns)
    } else {
        None
    };

    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let playback = state.playback.clone();
    let license = state.license.clone();
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
                            let is_v2 = dev.airplay_version.as_deref() == Some("2");
                            if is_v2 && tune_core::outputs::airplay2::daemon_available() {
                                let ap_dev_id = dev
                                    .capabilities
                                    .get("deviceid")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let ap2 = tune_core::outputs::airplay2::Airplay2Output::new(
                                    dev.name.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                    dev.id.clone(),
                                    ap_dev_id,
                                );
                                info!(name = %dev.name, "airplay2_output_registered");
                                (Some(Box::new(ap2)), "airplay2")
                            } else {
                                let ap = tune_core::outputs::airplay::AirplayOutput::new(
                                    dev.name.clone(),
                                    dev.id.clone(),
                                    dev.host.clone(),
                                    dev.port,
                                );
                                (Some(Box::new(ap)), "airplay")
                            }
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
                        #[cfg(feature = "oaat")]
                        OutputType::Oaat => {
                            // Premium gate: OAAT Protocol requires Premium
                            if !license
                                .check_feature(tune_core::license::Feature::OaatProtocol)
                                .await
                            {
                                info!(
                                    name = %dev.name,
                                    "oaat_zone_blocked_premium_required"
                                );
                                continue;
                            }
                            let oaat = tune_core::outputs::oaat::OaatOutput::new(
                                dev.name.clone(),
                                dev.host.clone(),
                                dev.port,
                                dev.id.clone(),
                            );
                            (Some(Box::new(oaat)), "oaat")
                        }
                        #[cfg(not(feature = "oaat"))]
                        OutputType::Oaat => {
                            tracing::warn!("OAAT support not compiled in");
                            (None, "oaat")
                        }
                        OutputType::Squeezebox => {
                            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(
                                db.clone(),
                            );
                            let current = settings
                                .get("lms_host")
                                .ok()
                                .flatten()
                                .or_else(|| settings.get("squeezebox_host").ok().flatten())
                                .unwrap_or_default();
                            if current.is_empty() {
                                // Use the CLI port (9090), NOT the JSON-RPC port (9000)
                                let cli_port = dev.port;
                                let lms_addr = format!("{}:{}", dev.host, cli_port);
                                // Write to both keys: "lms_host" is what the web client reads,
                                // "squeezebox_host" is legacy
                                settings.set("lms_host", &lms_addr).ok();
                                settings.set("squeezebox_host", &lms_addr).ok();
                                settings.set("squeezebox_enabled", "true").ok();
                                info!(host = %lms_addr, "mdns_lms_discovered_auto_configured");
                            }
                            (None, "squeezebox")
                        }
                        _ => (None, ""),
                    };

                    if let Some(output) = output {
                        let mut reg = outputs.lock().await;
                        reg.register(output);
                        info!(name = %dev.name, host = %dev.host, port = dev.port, r#type = output_type_str, "mdns_output_registered");

                        let zone_repo =
                            tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
                        if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
                            set_zone_online(&event_bus, &db, &dev.id, true);
                            if let Some(zone_id) = zone.id {
                                let vol = zone.volume as f64 / 100.0;
                                playback.set_volume(zone_id, vol).await;
                            }
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_reconnected");
                            event_bus.emit(
                                "device.reconnected",
                                serde_json::json!({
                                    "device_id": &dev.id,
                                    "name": &dev.name,
                                }),
                            );
                        } else {
                            let existing = zone_repo.list().unwrap_or_default();

                            // When a higher-priority protocol discovers a device at the
                            // same host as an existing zone (e.g. BluOS vs AirPlay for a
                            // Bluesound Node), upgrade the zone to the better protocol
                            // instead of creating a duplicate.
                            let upgrade_zone = existing.iter().find(|z| {
                                if let Some(ref old_dev_id) = z.output_device_id {
                                    // Match by host: device IDs are formatted as
                                    // "{type}-{host}-{port}", extract the host part
                                    // from the existing zone's device_id.
                                    let old_host = old_dev_id.splitn(3, '-').nth(1).unwrap_or("");
                                    let is_same_host = old_host == dev.host;
                                    if !is_same_host {
                                        return false;
                                    }
                                    // Only upgrade if new protocol has higher priority
                                    let old_prio = match z.output_type.as_deref() {
                                        Some("oaat") => OutputType::Oaat.priority(),
                                        Some("openhome") => OutputType::Openhome.priority(),
                                        Some("bluos") => OutputType::Bluos.priority(),
                                        Some("squeezebox") => OutputType::Squeezebox.priority(),
                                        Some("dlna") => OutputType::Dlna.priority(),
                                        Some("chromecast") => OutputType::Chromecast.priority(),
                                        Some("airplay") => OutputType::Airplay.priority(),
                                        _ => 0,
                                    };
                                    dev.device_type.priority() > old_prio
                                } else {
                                    false
                                }
                            });

                            if let Some(z) = upgrade_zone
                                && let Some(zid) = z.id
                            {
                                // Remove the old lower-priority output
                                if let Some(ref old_dev_id) = z.output_device_id {
                                    reg.remove(old_dev_id);
                                }
                                let _ = zone_repo.update_output_device(zid, &dev.id);
                                let _ = zone_repo.update_output_type(zid, output_type_str);
                                set_zone_online(&event_bus, &db, &dev.id, true);
                                info!(
                                    name = %dev.name,
                                    id = %dev.id,
                                    old_id = ?z.output_device_id,
                                    old_type = ?z.output_type,
                                    new_type = output_type_str,
                                    "mdns_zone_upgraded_to_higher_priority"
                                );
                            } else {
                                // Check if a zone with the same name exists but different
                                // device_id (device_id may have changed after a firmware
                                // update / re-pairing).
                                let same_name_zone = existing.iter().find(|z| z.name == dev.name);
                                if let Some(z) = same_name_zone
                                    && let Some(zid) = z.id
                                {
                                    let _ = zone_repo.update_output_device(zid, &dev.id);
                                    let _ = zone_repo.update_output_type(zid, output_type_str);
                                    set_zone_online(&event_bus, &db, &dev.id, true);
                                    info!(name = %dev.name, id = %dev.id, old_id = ?z.output_device_id, "mdns_zone_device_updated");
                                } else {
                                    // Premium gate: check zone limit before auto-creating
                                    let zone_count = zone_repo.count_online().unwrap_or(0);
                                    if !license.check_zone_limit(zone_count).await {
                                        info!(
                                            name = %dev.name,
                                            zone_count,
                                            "mdns_zone_creation_blocked_free_tier_limit"
                                        );
                                    } else {
                                        match zone_repo.get_or_create(
                                            &dev.name,
                                            Some(output_type_str),
                                            &dev.id,
                                        ) {
                                            Ok((zid, true)) => {
                                                event_bus.emit_typed(
                                                    EventType::ZoneCreated,
                                                    serde_json::json!({
                                                        "zone_id": zid,
                                                        "name": dev.name,
                                                        "device_id": dev.id,
                                                        "type": output_type_str,
                                                    }),
                                                );
                                                info!(name = %dev.name, zone_id = zid, r#type = output_type_str, "mdns_zone_auto_created");
                                            }
                                            Ok((zid, false)) => {
                                                set_zone_online(&event_bus, &db, &dev.id, true);
                                                info!(name = %dev.name, zone_id = zid, "mdns_zone_already_existed");
                                            }
                                            Err(e) => {
                                                tracing::warn!(name = %dev.name, device = %dev.id, error = %e, "mdns_zone_create_failed");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                MdnsEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    reg.remove(&id);
                    drop(reg);
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus
                        .emit_typed(EventType::DeviceLost, serde_json::json!({ "device_id": id }));
                    info!(id = %id, "mdns_output_removed_zone_offline");
                }
            }
        }
    });

    // After 15s, check for AirPlay zones whose host also speaks BluOS.
    // This catches Bluesound/NAD devices where _musc._tcp mDNS browse
    // didn't fire (common on Windows when multicast is partially blocked).
    {
        let outputs = state.outputs.clone();
        let db = state.backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            probe_airplay_for_bluos(&outputs, &db).await;
        });
    }

    handle
}

/// For every AirPlay zone, probe port 11000 to see if the device supports
/// BluOS.  If so, register a BluOS output and upgrade the zone.
async fn probe_airplay_for_bluos(
    outputs: &Arc<tokio::sync::Mutex<OutputRegistry>>,
    db: &Arc<dyn DbBackend>,
) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let zones = zone_repo.list().unwrap_or_default();

    let airplay_zones: Vec<_> = zones
        .iter()
        .filter(|z| z.output_type.as_deref() == Some("airplay"))
        .collect();

    if airplay_zones.is_empty() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();

    for z in airplay_zones {
        let Some(ref dev_id) = z.output_device_id else {
            continue;
        };
        // Extract host from mDNS device_id "airplay-{host}-{port}"
        let host = match dev_id.splitn(3, '-').nth(1) {
            Some(h) if !h.is_empty() => h,
            _ => continue,
        };

        // Skip if a BluOS zone already exists for this host
        let already_bluos = zones.iter().any(|zz| {
            zz.output_type.as_deref() == Some("bluos")
                && zz
                    .output_device_id
                    .as_deref()
                    .map_or(false, |id| id.splitn(3, '-').nth(1) == Some(host))
        });
        if already_bluos {
            continue;
        }

        let probe_url = format!("http://{host}:11000/Status");
        match client.get(&probe_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let bluos_id = format!("bluos-{host}-11000");
                let bluos = tune_core::outputs::bluos::BluosOutput::new(
                    z.name.clone(),
                    bluos_id.clone(),
                    host.to_string(),
                    11000,
                );

                let mut reg = outputs.lock().await;
                // Remove the old AirPlay output
                reg.remove(dev_id);
                reg.register(Box::new(bluos));
                drop(reg);

                if let Some(zid) = z.id {
                    let _ = zone_repo.update_output_device(zid, &bluos_id);
                    let _ = zone_repo.update_output_type(zid, "bluos");
                    let _ = zone_repo.set_online_by_device(&bluos_id, true);
                }

                info!(
                    name = %z.name,
                    host = host,
                    old_id = %dev_id,
                    new_id = %bluos_id,
                    "bluos_fallback_probe_upgraded_airplay_zone"
                );
            }
            _ => {
                tracing::debug!(host = host, name = %z.name, "bluos_fallback_probe_no_response");
            }
        }
    }
}
