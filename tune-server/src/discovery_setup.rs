use std::sync::Arc;

use tracing::info;

use tune_core::db::backend::DbBackend;
use tune_core::outputs::OutputRegistry;
use tune_core::outputs::oh_events::OpenHomeEventListener;

use tune_core::event_bus::EventBus;
use tune_core::event_types::EventType;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Resolve a UPnP service `controlURL` (from the device description) into an
/// absolute URL usable by the SOAP client.
///
/// The controlURL may be **relative** (`/MediaRenderer/AVTransport/Control`, the
/// common case) or already **absolute** (`http://host:port/...`). Frontier Silicon
/// radios (Ruark R3, Stream 94i Plus) advertise an absolute controlURL; blindly
/// prefixing `http://host:port` yielded `http://host:PORThttp://...` — the port
/// token became `PORThttp`, the URL failed to parse, and every SOAP call died with
/// `soap send: builder error` (Yves: no sound, UI stuck on "loading title"). This
/// mirrors the MediaServer handling in `ssdp.rs`.
fn resolve_control_url(host: &str, port: u16, control_url: &str) -> String {
    if control_url.starts_with("http://") || control_url.starts_with("https://") {
        control_url.to_string()
    } else {
        let sep = if control_url.starts_with('/') {
            ""
        } else {
            "/"
        };
        format!("http://{host}:{port}{sep}{control_url}")
    }
}

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
        let mut seen_hosts: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                        &mut seen_hosts,
                    )
                    .await;
                }
                SsdpEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    // DLNA/OpenHome tolerance: a Samsung TV (and similar SOAP
                    // renderers) stops advertising on SSDP when it goes idle, yet
                    // still answers an AVTransport command if woken. Keep its
                    // output in the registry so a play attempt can wake it — the
                    // offline gate already allows an offline-but-registered zone
                    // (Bilou's "erreur 503 en DLNA"). Registration is idempotent,
                    // so a later re-advertise overwrites it with fresh URLs. The
                    // zone is still flagged offline for the UI. Non-SOAP outputs
                    // (chromecast, …) are dropped as before.
                    let soap_wakeable =
                        matches!(reg.type_of(&id).as_deref(), Some("dlna") | Some("openhome"));
                    if !soap_wakeable {
                        reg.remove(&id);
                    }
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus.emit_typed(
                        EventType::DeviceLost,
                        serde_json::json!({ "device_id": id }),
                    );
                    info!(id = %id, kept_registered = soap_wakeable, "device_lost_zone_offline");
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
    _license: &Arc<tune_core::license::LicenseManager>,
    seen_hosts: &mut std::collections::HashSet<String>,
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
        // Resolve each controlURL to an absolute URL (see `resolve_control_url`):
        // relative paths are joined onto host:port, absolute URLs kept as-is —
        // otherwise Frontier Silicon radios (Ruark, Stream 94i) fail with
        // `soap send: builder error` and play nothing.
        let av_url = svc_urls
            .get("avtransport")
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
        let rc_url = svc_urls
            .get("renderingcontrol")
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
        let cm_url = svc_urls
            .get("connectionmanager")
            .or_else(|| svc_urls.get("ConnectionManager"))
            .map(|p| resolve_control_url(&dev.host, dev.port, p));
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
    if zone_repo.is_device_hidden(&dev.id) {
        tracing::debug!(name = %dev.name, id = %dev.id, "ssdp_zone_hidden_skipping");
        return;
    }
    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
        seen_hosts.insert(dev.host.clone());
        set_zone_online(event_bus, db, &dev.id, true);
        if let Some(zone_id) = zone.id {
            let vol = zone.volume as f64 / 100.0;
            playback.set_volume(zone_id, vol).await;
            // Backfill the host on zones created before host-based dedup existed,
            // so a later UUID change (Denon restart) can reconnect by host (#942).
            let _ = zone_repo.set_host(zone_id, &dev.host);
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
    } else if let Some(existing_zone_id) = zone_repo.zone_id_by_host(&dev.host) {
        // Same physical renderer, NEW UPnP UUID (e.g. Denon Ceol N12 after a
        // restart, which changes its UUIDs): re-point the existing zone to the
        // live device_id instead of spawning a duplicate. This keeps the zone's
        // per-zone settings — crucially the "native FLAC" toggle and volume —
        // on the one zone the user actually plays (forum #942: duplicate Denon
        // zones meant the toggle was set on a zone that was never the one
        // playing, so Tidal FLAC kept being transcoded to WAV).
        seen_hosts.insert(dev.host.clone());
        let _ = zone_repo.update_device_id(existing_zone_id, &dev.id);
        let _ = zone_repo.set_host(existing_zone_id, &dev.host);
        set_zone_online(event_bus, db, &dev.id, true);
        let vol = zone_repo
            .get(existing_zone_id)
            .ok()
            .flatten()
            .map(|z| z.volume as f64 / 100.0);
        if let Some(vol) = vol {
            playback.set_volume(existing_zone_id, vol).await;
        }
        info!(
            name = %dev.name,
            id = %dev.id,
            host = %dev.host,
            zone_id = existing_zone_id,
            "zone_device_reconnected_by_host"
        );
        event_bus.emit(
            "device.reconnected",
            serde_json::json!({
                "device_id": &dev.id,
                "name": &dev.name,
                "host": &dev.host,
            }),
        );
    } else if !is_tv {
        // Check zone_auto_create setting
        let auto_create = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get("zone_auto_create")
            .ok()
            .flatten()
            .map(|v| v != "false")
            .unwrap_or(true);
        if !auto_create {
            info!(name = %dev.name, id = %dev.id, "ssdp_zone_auto_create_disabled_skipping");
            return;
        }

        // Auto-created zones start dormant; the free-tier cap is enforced at
        // first play (orchestrator.play), so discovery always registers.

        // Dedup by host: skip if we already created a zone for this IP
        // (e.g. Denon AVR exposes 5 UPnP services with different UUIDs
        // but the same host — only the first one should create a zone)
        if !seen_hosts.insert(dev.host.clone()) {
            tracing::debug!(name = %dev.name, host = %dev.host, id = %dev.id, "ssdp_zone_host_already_seen_skipping");
            return;
        }

        let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
        let existing_zones = zone_repo.list().unwrap_or_default();
        let name_taken = existing_zones.iter().any(|z| z.name == short_name);
        let zone_name = if name_taken {
            dev.name.clone()
        } else {
            short_name.to_string()
        };

        // Persisted name dedup: a device exposing several UPnP services (a Sonos
        // is both a DLNA MediaRenderer and OpenHome) can be rediscovered under a
        // NEW device_id after a restart. get_by_device_id above misses it and
        // the per-pass seen_hosts is empty, so a duplicate zone with the SAME
        // disambiguated name (which includes the renderer UUID) gets created
        // (Bertrand: "Chambre - Sonos … RINCON…" ×2). Same full name = same
        // physical device → skip the duplicate; its live device_id reconnects
        // via the get_by_device_id path.
        if existing_zones.iter().any(|z| z.name == zone_name) {
            tracing::debug!(name = %zone_name, id = %dev.id, host = %dev.host, "ssdp_zone_name_exists_skipping_duplicate");
            return;
        }

        let type_str = if dev.device_type == tune_core::discovery::device::OutputType::Openhome {
            "openhome"
        } else {
            "dlna"
        };
        match zone_repo.get_or_create(&zone_name, Some(type_str), &dev.id) {
            Ok((zid, true)) => {
                // Persist the host so a later UUID change reconnects here (#942).
                let _ = zone_repo.set_host(zid, &dev.host);
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
                let _ = zone_repo.set_host(zid, &dev.host);
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
    tokio::spawn(async move {
        use tune_core::discovery::device::OutputType;
        use tune_core::discovery::mdns::MdnsEvent;
        while let Some(event) = mdns_rx.recv().await {
            match event {
                MdnsEvent::DeviceDiscovered(dev) | MdnsEvent::DeviceUpdated(dev) => {
                    // Set when an AirPlay 2 device falls back to the legacy
                    // AirPlay output (daemon unavailable / deviceid unknown):
                    // the output is registered but no zone is auto-created.
                    let mut airplay_v2_fallback = false;
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
                            // The AirPlay deviceid (a MAC) is stored on `mac_address`
                            // by the mDNS handler; `capabilities["deviceid"]` is never
                            // populated. Reading only the latter left ap_device_id empty,
                            // so the AirPlay 2 daemon rejected every connection ("MAC
                            // address must be 12 hex characters, got 0") — Matteo's Sonos
                            // Era 100 "Chambre Missou". Prefer mac_address, fall back to
                            // the capability.
                            let ap_dev_id = dev
                                .mac_address
                                .clone()
                                .filter(|s| !s.is_empty())
                                .or_else(|| {
                                    dev.capabilities
                                        .get("deviceid")
                                        .and_then(|v| v.as_str())
                                        .filter(|s| !s.is_empty())
                                        .map(str::to_string)
                                })
                                .unwrap_or_default();
                            // Without a device id the AirPlay 2 daemon can't connect, so
                            // a v2 output would be a dead zone. Fall back to legacy AirPlay
                            // in that case instead of registering a broken v2 zone.
                            if is_v2
                                && tune_core::outputs::airplay2::daemon_available()
                                && !ap_dev_id.is_empty()
                            {
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
                                if is_v2 {
                                    // An AirPlay 2 device served by the legacy
                                    // path is a dead end: these devices demand
                                    // the pairing only the airplay2 daemon can
                                    // perform, so every ANNOUNCE gets a 403
                                    // (forum #1183, Samsung S95BA TV). Register
                                    // the output so a manually created zone can
                                    // still target it, but never auto-create a
                                    // zone for it (#788 already intended "skip
                                    // v2 zone if daemon absent").
                                    airplay_v2_fallback = true;
                                }
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
                        // Honour a user deletion: deleting a zone soft-hides it
                        // (is_hidden=1). The SSDP handler already skips hidden
                        // devices; the mDNS handler did not, so an AirPlay/
                        // Chromecast/BluOS device (e.g. Fabien's Beosound Stage)
                        // was reconnected — or, with no device_id match, re-created
                        // via auto_create — at startup, resurrecting the deleted
                        // zone even with "create zones automatically" OFF. Skip the
                        // whole reconnect/create block for hidden devices.
                        if zone_repo.is_device_hidden(&dev.id) {
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_hidden_skipping");
                        } else if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev.id) {
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
                        } else if airplay_v2_fallback {
                            // No existing zone for this device and the legacy
                            // AirPlay fallback can never play on it (v2 pairing
                            // required → ANNOUNCE 403): don't auto-create a
                            // guaranteed-dead zone, and don't let it capture an
                            // existing same-name/same-host zone of another
                            // protocol either (forum #1183).
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_skipped_airplay_v2_fallback");
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
                                    // Cross-protocol dedup (forum #1183): the
                                    // same physical device may already be a
                                    // zone through another protocol — e.g. a
                                    // Samsung S95BA TV present as a (renamed)
                                    // DLNA zone that also announces AirPlay
                                    // over mDNS. Match against the output
                                    // registry (same name + same host across
                                    // protocols) AND against existing zones by
                                    // the REAL host of their registered output
                                    // (`host_of`, robust for DLNA `uuid:…`
                                    // device_ids the old "{type}-{host}-{port}"
                                    // parsing mangled) or by case-insensitive
                                    // name. Skip zone creation on conflict.
                                    let registry_conflicts = reg.conflicting_outputs_same_host(
                                        &dev.name,
                                        output_type_str,
                                        &dev.host,
                                    );
                                    let zone_conflict = find_cross_protocol_zone_conflict(
                                        &existing,
                                        |id| reg.host_of(id),
                                        &dev.name,
                                        &dev.host,
                                        output_type_str,
                                    );
                                    if !registry_conflicts.is_empty() || zone_conflict.is_some() {
                                        info!(
                                            name = %dev.name,
                                            id = %dev.id,
                                            host = %dev.host,
                                            r#type = output_type_str,
                                            registry_conflicts = ?registry_conflicts,
                                            conflicting_zone = ?zone_conflict.map(|z| z.name.as_str()),
                                            "mdns_zone_skipped_conflicting_protocol"
                                        );
                                    } else {
                                        // Check zone_auto_create setting
                                        let auto_create =
                                        tune_core::db::settings_repo::SettingsRepo::with_backend(
                                            db.clone(),
                                        )
                                        .get("zone_auto_create")
                                        .ok()
                                        .flatten()
                                        .map(|v| v != "false")
                                        .unwrap_or(true);
                                        if !auto_create {
                                            info!(name = %dev.name, id = %dev.id, "mdns_zone_auto_create_disabled_skipping");
                                        } else {
                                            // Auto-created zones start dormant; the
                                            // free-tier cap is enforced at first play.
                                            {
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
                                                        set_zone_online(
                                                            &event_bus, &db, &dev.id, true,
                                                        );
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
                    }
                }
                MdnsEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    reg.remove(&id);
                    drop(reg);
                    set_zone_online(&event_bus, &db, &id, false);
                    event_bus.emit_typed(
                        EventType::DeviceLost,
                        serde_json::json!({ "device_id": id }),
                    );
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

    let client = tune_core::http::client::builder()
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

/// An existing zone that already exposes the same physical device through a
/// DIFFERENT protocol, if any. Used by the mDNS auto-create path to avoid
/// presenting one device as two zones (forum #1183: a Samsung S95BA TV already
/// present as a renamed DLNA zone also announces AirPlay over mDNS; the
/// auto-created legacy AirPlay zone was dead — the TV requires AirPlay 2
/// pairing).
///
/// A zone conflicts when its `output_type` differs from `new_type` AND either:
/// - the REAL host of its registered output equals `dev_host` — the host is
///   resolved through `resolve_host` (normally [`OutputRegistry::host_of`],
///   which recorded the output's `host()` at registration). This is robust for
///   DLNA zones whose `output_device_id` is a `uuid:…` string, which the old
///   `"{type}-{host}-{port}"` `splitn` parsing mangled into a UUID fragment; or
/// - the zone name equals the device name case-insensitively (a same-name
///   zone of another protocol, even if its output isn't registered yet).
///
/// [`OutputRegistry::host_of`]: tune_core::outputs::registry::OutputRegistry::host_of
fn find_cross_protocol_zone_conflict<'a>(
    zones: &'a [tune_core::db::zone_repo::Zone],
    resolve_host: impl Fn(&str) -> Option<String>,
    dev_name: &str,
    dev_host: &str,
    new_type: &str,
) -> Option<&'a tune_core::db::zone_repo::Zone> {
    zones.iter().find(|z| {
        let zone_type = z.output_type.as_deref().unwrap_or("");
        if zone_type.is_empty() || zone_type.eq_ignore_ascii_case(new_type) {
            return false;
        }
        let same_host = z
            .output_device_id
            .as_deref()
            .and_then(&resolve_host)
            .is_some_and(|h| h.eq_ignore_ascii_case(dev_host));
        same_host || z.name.eq_ignore_ascii_case(dev_name)
    })
}

/// Register outputs coming from [`OutputProvider`]s handed to
/// `run::main_blocking` — the seam for out-of-tree output crates (e.g. the
/// private tune-diretta) that cannot appear in the public dependency graph.
///
/// Polls `discover()` at startup and then every 60 s (providers have no push
/// discovery the way SSDP/mDNS do), registers unseen device_ids, and applies
/// the same zone lifecycle as the mDNS handler: hidden zones stay deleted,
/// known device_ids reconnect (online + restored volume), renamed device_ids
/// re-attach by zone name, and new devices honour `zone_auto_create`.
pub fn spawn_output_providers(
    state: &AppState,
    providers: Vec<Arc<dyn tune_core::outputs::traits::OutputProvider>>,
) {
    if providers.is_empty() {
        return;
    }
    let outputs = state.outputs.clone();
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let playback = state.playback.clone();
    let license = state.license.clone();

    tokio::spawn(async move {
        loop {
            // Rebuilt every poll so a module bought (or refunded) mid-session
            // takes effect at the next discovery pass, without a restart.
            let ctx = tune_core::outputs::traits::ProviderContext {
                licensed_modules: license.modules().await,
            };
            for provider in &providers {
                for output in provider.discover(&ctx).await {
                    let dev_id = output.device_id().to_string();
                    let name = output.name().to_string();
                    let otype = output.output_type().to_string();
                    {
                        let mut reg = outputs.lock().await;
                        if reg.contains(&dev_id) {
                            continue;
                        }
                        reg.register(output);
                    }
                    info!(
                        provider = provider.provider_name(),
                        name = %name,
                        id = %dev_id,
                        r#type = %otype,
                        "provider_output_registered"
                    );

                    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone());
                    if zone_repo.is_device_hidden(&dev_id) {
                        info!(name = %name, id = %dev_id, "provider_zone_hidden_skipping");
                        continue;
                    }
                    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&dev_id) {
                        set_zone_online(&event_bus, &db, &dev_id, true);
                        if let Some(zone_id) = zone.id {
                            let vol = zone.volume as f64 / 100.0;
                            playback.set_volume(zone_id, vol).await;
                        }
                        info!(name = %name, id = %dev_id, "provider_zone_reconnected");
                        event_bus.emit(
                            "device.reconnected",
                            serde_json::json!({
                                "device_id": &dev_id,
                                "name": &name,
                            }),
                        );
                    } else {
                        // Device_id may have changed (firmware update / re-pairing):
                        // re-attach an existing zone by name before creating one.
                        let existing = zone_repo.list().unwrap_or_default();
                        if let Some(z) = existing.iter().find(|z| z.name == name)
                            && let Some(zid) = z.id
                        {
                            let _ = zone_repo.update_output_device(zid, &dev_id);
                            let _ = zone_repo.update_output_type(zid, &otype);
                            set_zone_online(&event_bus, &db, &dev_id, true);
                            info!(name = %name, id = %dev_id, old_id = ?z.output_device_id, "provider_zone_device_updated");
                        } else {
                            let auto_create =
                                tune_core::db::settings_repo::SettingsRepo::with_backend(
                                    db.clone(),
                                )
                                .get("zone_auto_create")
                                .ok()
                                .flatten()
                                .map(|v| v != "false")
                                .unwrap_or(true);
                            if !auto_create {
                                info!(name = %name, id = %dev_id, "provider_zone_auto_create_disabled_skipping");
                            } else {
                                match zone_repo.get_or_create(&name, Some(&otype), &dev_id) {
                                    Ok((zid, true)) => {
                                        event_bus.emit_typed(
                                            EventType::ZoneCreated,
                                            serde_json::json!({
                                                "zone_id": zid,
                                                "name": name,
                                            }),
                                        );
                                        set_zone_online(&event_bus, &db, &dev_id, true);
                                        info!(name = %name, id = %dev_id, zone_id = zid, "provider_zone_created");
                                    }
                                    Ok((_, false)) => {
                                        set_zone_online(&event_bus, &db, &dev_id, true);
                                    }
                                    Err(e) => {
                                        tracing::warn!(name = %name, id = %dev_id, error = %e, "provider_zone_create_failed");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{find_cross_protocol_zone_conflict, resolve_control_url};
    use tune_core::db::zone_repo::Zone;

    fn zone(name: &str, output_type: &str, device_id: &str) -> Zone {
        Zone {
            id: Some(1),
            name: name.to_string(),
            output_type: Some(output_type.to_string()),
            output_device_id: Some(device_id.to_string()),
            volume: 50,
            muted: false,
            online: true,
            gapless_enabled: true,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume: false,
            autoplay_enabled: false,
        }
    }

    /// Forum #1183: a Samsung S95BA TV is already a DLNA zone — renamed by the
    /// user, with a `uuid:…` device_id (so neither the exact-name dedup nor the
    /// old `splitn('-')` host extraction can match it) — and then announces
    /// AirPlay over mDNS at the same host. The conflict must be detected via
    /// the output registry's real host so no second (dead) zone is created.
    #[test]
    fn dlna_uuid_zone_conflicts_with_airplay_arrival_on_same_host() {
        let zones = vec![zone(
            "TV Salon", // renamed: name no longer matches the device name
            "dlna",
            "uuid:3a4eedf4-1bf0-4c9a-9c2b-0123456789ab",
        )];
        let resolve = |id: &str| {
            (id == "uuid:3a4eedf4-1bf0-4c9a-9c2b-0123456789ab").then(|| "192.168.1.42".to_string())
        };
        let hit = find_cross_protocol_zone_conflict(
            &zones,
            resolve,
            "Samsung S95BA",
            "192.168.1.42",
            "airplay",
        );
        assert_eq!(hit.map(|z| z.name.as_str()), Some("TV Salon"));

        // A different host is NOT a conflict.
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Samsung S95BA",
                "192.168.1.99",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn same_protocol_never_conflicts() {
        // Two AirPlay devices on the same host (e.g. an AV receiver exposing
        // several inputs) must not dedup against their own protocol.
        let zones = vec![zone("Ampli HC", "airplay", "airplay-192.168.1.42-7000")];
        let resolve = |_: &str| Some("192.168.1.42".to_string());
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Ampli HC Zone 2",
                "192.168.1.42",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn case_insensitive_name_conflicts_even_without_registered_output() {
        // The zone's output isn't registered (host unresolvable), but the name
        // matches case-insensitively across protocols → still a conflict.
        let zones = vec![zone("Samsung S95BA", "dlna", "uuid:dead-beef")];
        let resolve = |_: &str| None;
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "SAMSUNG s95ba",
                "192.168.1.42",
                "airplay",
            )
            .is_some()
        );
        // Different name + unresolvable host → no conflict.
        assert!(
            find_cross_protocol_zone_conflict(
                &zones,
                resolve,
                "Chambre",
                "192.168.1.42",
                "airplay",
            )
            .is_none()
        );
    }

    #[test]
    fn relative_control_url_joins_host_port() {
        // The common case: a leading-slash relative path (Denon DMP-A10, LHC).
        assert_eq!(
            resolve_control_url("192.168.68.50", 8080, "/MediaRenderer/AVTransport/Control"),
            "http://192.168.68.50:8080/MediaRenderer/AVTransport/Control"
        );
    }

    #[test]
    fn relative_without_leading_slash_gets_one() {
        assert_eq!(
            resolve_control_url("10.0.0.2", 55000, "upnp/control/AVTransport"),
            "http://10.0.0.2:55000/upnp/control/AVTransport"
        );
    }

    #[test]
    fn absolute_control_url_kept_as_is() {
        // Frontier Silicon radios (Ruark R3, Stream 94i Plus) advertise an
        // absolute controlURL. It must be used verbatim — prefixing host:port
        // would yield `http://host:PORThttp://...` (invalid port) → the reqwest
        // `soap send: builder error` that left Yves with no sound.
        let abs = "http://192.168.68.55:8080/dev0/srv1/control";
        assert_eq!(resolve_control_url("192.168.68.55", 8080, abs), abs);
        let abs_https = "https://192.168.68.55:443/control";
        assert_eq!(
            resolve_control_url("192.168.68.55", 443, abs_https),
            abs_https
        );
    }
}
