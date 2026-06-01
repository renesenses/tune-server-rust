use std::sync::Arc;

use tracing::info;

use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::OutputRegistry;
use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

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
    let db = state.db.clone();
    let config = config.clone();
    let event_bus = state.event_bus.clone();
    tokio::spawn(async move {
        use tune_core::discovery::ssdp::SsdpEvent;
        while let Some(event) = ssdp_rx.recv().await {
            match event {
                SsdpEvent::DeviceDiscovered(dev) => {
                    handle_ssdp_discovered(&dev, &outputs, &db, &config, &event_bus, &oh_listener)
                        .await;
                }
                SsdpEvent::DeviceLost(id) => {
                    let mut reg = outputs.lock().await;
                    reg.remove(&id);
                    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(db.clone());
                    let _ = zone_repo.set_online_by_device(&id, false);
                    info!(id = %id, "output_removed_zone_offline");
                }
            }
        }
    });
}

async fn handle_ssdp_discovered(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    outputs: &Arc<tokio::sync::Mutex<OutputRegistry>>,
    db: &SqliteDb,
    config: &TuneConfig,
    event_bus: &Arc<tune_core::event_bus::EventBus>,
    oh_listener: &Option<Arc<OpenHomeEventListener>>,
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
        if let (Some(av), Some(rc)) = (av_url, rc_url) {
            let delay = config.play_delay_for(&dev.name);
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

    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(db.clone());
    let existing = zone_repo.list().unwrap_or_default();
    let already = existing
        .iter()
        .any(|z| z.output_device_id.as_deref() == Some(&dev.id));
    if already {
        let _ = zone_repo.set_online_by_device(&dev.id, true);
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
        let short_name = dev.name.split(" - ").next().unwrap_or(&dev.name);
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
        if let Ok(zid) = zone_repo.create(&zone_name, Some(type_str), Some(&dev.id)) {
            info!(name = %zone_name, zone_id = zid, device = %dev.id, r#type = type_str, "zone_auto_created");
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
        Some(mdns)
    } else {
        None
    };

    let outputs = state.outputs.clone();
    let db = state.db.clone();
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
                            let ap = tune_core::outputs::airplay::AirplayOutput::new(
                                dev.name.clone(),
                                dev.id.clone(),
                                dev.host.clone(),
                                dev.port,
                            );
                            (Some(Box::new(ap)), "airplay")
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
                            let settings =
                                tune_core::db::settings_repo::SettingsRepo::new(db.clone());
                            let current = settings
                                .get("squeezebox_host")
                                .ok()
                                .flatten()
                                .unwrap_or_default();
                            if current.is_empty() {
                                let lms_addr = format!("{}:9000", dev.host);
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

                        let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(db.clone());
                        let existing = zone_repo.list().unwrap_or_default();
                        let already_by_device = existing
                            .iter()
                            .any(|z| z.output_device_id.as_deref() == Some(&dev.id));
                        if already_by_device {
                            let _ = zone_repo.set_online_by_device(&dev.id, true);
                            info!(name = %dev.name, id = %dev.id, "mdns_zone_reconnected");
                        } else {
                            let name_taken = existing.iter().any(|z| z.name == dev.name);
                            if !name_taken {
                                if let Ok(zid) = zone_repo.create(
                                    &dev.name,
                                    Some(output_type_str),
                                    Some(&dev.id),
                                ) {
                                    info!(name = %dev.name, zone_id = zid, r#type = output_type_str, "mdns_zone_auto_created");
                                }
                            } else {
                                info!(name = %dev.name, r#type = output_type_str, "mdns_zone_skipped_name_exists");
                            }
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

    handle
}
