use std::collections::HashMap;
use std::sync::Arc;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::device::{DiscoveredDevice, OutputType};

pub const AIRPLAY_SERVICE: &str = "_raop._tcp.local.";
pub const AIRPLAY2_SERVICE: &str = "_airplay._tcp.local.";
pub const BLUOS_SERVICE: &str = "_musc._tcp.local.";
pub const CHROMECAST_SERVICE: &str = "_googlecast._tcp.local.";
pub const SQUEEZEBOX_SERVICE: &str = "_slimcli._tcp.local.";
pub const TUNE_SERVICE: &str = "_tune-server._tcp.local.";
pub const OAAT_SERVICE: &str = "_oaat._tcp.local.";

#[derive(Debug, Clone)]
pub enum MdnsEvent {
    DeviceDiscovered(DiscoveredDevice),
    DeviceLost(String),
    DeviceUpdated(DiscoveredDevice),
}

#[derive(Debug, Clone)]
pub struct MdnsServiceConfig {
    pub service_type: String,
    pub output_type: OutputType,
    pub default_port: u16,
}

pub struct MdnsScanner {
    daemon: ServiceDaemon,
    configs: Vec<MdnsServiceConfig>,
    state: Arc<Mutex<MdnsState>>,
    event_tx: mpsc::Sender<MdnsEvent>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct MdnsState {
    devices: HashMap<String, DiscoveredDevice>,
    service_to_device: HashMap<String, String>,
}

impl MdnsState {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            service_to_device: HashMap::new(),
        }
    }
}

impl MdnsScanner {
    pub fn new(event_tx: mpsc::Sender<MdnsEvent>) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS daemon: {e}"))?;

        Ok(Self {
            daemon,
            configs: Vec::new(),
            state: Arc::new(Mutex::new(MdnsState::new())),
            event_tx,
            tasks: Vec::new(),
        })
    }

    pub fn with_airplay(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: AIRPLAY_SERVICE.to_string(),
            output_type: OutputType::Airplay,
            default_port: 7000,
        });
        self.configs.push(MdnsServiceConfig {
            service_type: AIRPLAY2_SERVICE.to_string(),
            output_type: OutputType::Airplay,
            default_port: 7000,
        });
        self
    }

    pub fn with_bluos(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: BLUOS_SERVICE.to_string(),
            output_type: OutputType::Bluos,
            default_port: 11000,
        });
        self
    }

    pub fn with_chromecast(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: CHROMECAST_SERVICE.to_string(),
            output_type: OutputType::Chromecast,
            default_port: 8009,
        });
        self
    }

    pub fn with_squeezebox(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: SQUEEZEBOX_SERVICE.to_string(),
            output_type: OutputType::Squeezebox,
            default_port: 9090,
        });
        self
    }

    pub fn with_oaat(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: OAAT_SERVICE.to_string(),
            output_type: OutputType::Oaat,
            default_port: 9740,
        });
        self
    }

    pub fn with_tune_peers(mut self) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type: TUNE_SERVICE.to_string(),
            output_type: OutputType::Local,
            default_port: 8888,
        });
        self
    }

    pub fn with_service(
        mut self,
        service_type: String,
        output_type: OutputType,
        default_port: u16,
    ) -> Self {
        self.configs.push(MdnsServiceConfig {
            service_type,
            output_type,
            default_port,
        });
        self
    }

    /// Announce this Tune server instance via mDNS so HomeAssistant
    /// and other clients can auto-discover it.
    pub fn register_self(&self, port: u16, version: &str) -> Result<(), String> {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "tune-server".into());
        let service_name = format!("Tune ({})", hostname);

        let local_ip = crate::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());

        let properties = [("version", version), ("path", "/api/v1")];

        let svc = ServiceInfo::new(
            TUNE_SERVICE,
            &service_name,
            &format!("{hostname}.local."),
            &local_ip,
            port,
            &properties[..],
        )
        .map_err(|e| format!("mDNS register: {e}"))?;

        self.daemon
            .register(svc)
            .map_err(|e| format!("mDNS register: {e}"))?;

        info!(
            service = TUNE_SERVICE,
            name = %service_name,
            ip = %local_ip,
            port,
            "mdns_service_registered"
        );
        Ok(())
    }

    pub fn start(&mut self) -> Result<(), String> {
        let mut has_bluos = false;
        for config in &self.configs {
            let receiver = self
                .daemon
                .browse(&config.service_type)
                .map_err(|e| format!("browse {}: {e}", config.service_type))?;

            if config.output_type == OutputType::Bluos {
                has_bluos = true;
            }

            let state = self.state.clone();
            let event_tx = self.event_tx.clone();
            let output_type = config.output_type;
            let default_port = config.default_port;
            let service_type = config.service_type.clone();

            let task = tokio::spawn(async move {
                browse_loop(
                    receiver,
                    state,
                    event_tx,
                    output_type,
                    default_port,
                    &service_type,
                )
                .await;
            });
            self.tasks.push(task);

            info!(service = %config.service_type, "mdns_browse_started");
        }

        // Diagnostic: warn after 30s if no BluOS device was discovered via mDNS.
        // Helps users diagnose mDNS issues (firewalls, VLANs, VPN blocking multicast).
        if has_bluos {
            let state = self.state.clone();
            self.tasks.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let st = state.lock().await;
                let bluos_count = st
                    .devices
                    .values()
                    .filter(|d| d.device_type == OutputType::Bluos)
                    .count();
                if bluos_count == 0 {
                    warn!(
                        "mdns_no_bluos_devices_found after 30s — if you have BluOS devices, \
                         check that mDNS/multicast is not blocked (firewall, VLAN, VPN). \
                         You can add devices manually via POST /api/v1/devices/add"
                    );
                }
            }));
        }

        Ok(())
    }

    pub fn stop(&mut self) {
        for config in &self.configs {
            let _ = self.daemon.stop_browse(&config.service_type);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        let _ = self.daemon.shutdown();
        info!("mdns_scanner_stopped");
    }

    pub async fn devices(&self) -> Vec<DiscoveredDevice> {
        let state = self.state.lock().await;
        state.devices.values().cloned().collect()
    }

    pub async fn device_count(&self) -> usize {
        let state = self.state.lock().await;
        state.devices.len()
    }
}

async fn browse_loop(
    receiver: mdns_sd::Receiver<ServiceEvent>,
    state: Arc<Mutex<MdnsState>>,
    event_tx: mpsc::Sender<MdnsEvent>,
    output_type: OutputType,
    default_port: u16,
    service_type: &str,
) {
    loop {
        match receiver.recv_async().await {
            Ok(event) => {
                handle_event(
                    event,
                    &state,
                    &event_tx,
                    output_type,
                    default_port,
                    service_type,
                )
                .await;
            }
            Err(e) => {
                debug!(error = %e, service = service_type, "mdns_recv_error");
                break;
            }
        }
    }
}

async fn handle_event(
    event: ServiceEvent,
    state: &Arc<Mutex<MdnsState>>,
    event_tx: &mpsc::Sender<MdnsEvent>,
    output_type: OutputType,
    default_port: u16,
    service_type: &str,
) {
    match event {
        ServiceEvent::ServiceResolved(info) => {
            let device = service_to_device(&info, output_type, default_port);
            let dev_id = device.id.clone();

            let mut st = state.lock().await;

            // For AirPlay: skip RAOP if AirPlay2 already registered for this host (or vice versa)
            if output_type == OutputType::Airplay {
                let host_already = st.devices.values().any(|d| {
                    d.device_type == OutputType::Airplay && d.host == device.host && d.id != dev_id
                });
                if host_already {
                    debug!(id = %dev_id, name = %device.name, host = %device.host, service = service_type, "mdns_airplay_dup_skipped");
                    drop(st);
                    return;
                }
            }

            let is_new = !st.devices.contains_key(&dev_id);
            st.service_to_device
                .insert(info.get_fullname().to_string(), dev_id.clone());
            st.devices.insert(dev_id.clone(), device.clone());
            drop(st);

            if is_new {
                info!(id = %dev_id, name = %device.name, service = service_type, "mdns_device_discovered");
                let _ = event_tx.send(MdnsEvent::DeviceDiscovered(device)).await;
            } else {
                debug!(id = %dev_id, "mdns_device_updated");
                let _ = event_tx.send(MdnsEvent::DeviceUpdated(device)).await;
            }
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            let mut st = state.lock().await;
            if let Some(dev_id) = st.service_to_device.remove(&fullname)
                && let Some(device) = st.devices.remove(&dev_id)
            {
                info!(id = %dev_id, name = %device.name, "mdns_device_lost");
                drop(st);
                let _ = event_tx.send(MdnsEvent::DeviceLost(dev_id)).await;
            }
        }
        ServiceEvent::SearchStarted(stype) => {
            debug!(service = %stype, "mdns_search_started");
        }
        ServiceEvent::SearchStopped(stype) => {
            debug!(service = %stype, "mdns_search_stopped");
        }
        _ => {}
    }
}

fn service_to_device(
    info: &ResolvedService,
    output_type: OutputType,
    default_port: u16,
) -> DiscoveredDevice {
    let raw_name = info
        .get_fullname()
        .split('.')
        .next()
        .unwrap_or(info.get_fullname())
        .replace('_', " ")
        .trim()
        .to_string();

    // RAOP names look like "800A805D4DEE@DMP-A8" — strip the hex MAC prefix
    let name = if output_type == OutputType::Airplay {
        if let Some(pos) = raw_name.find('@') {
            let after = &raw_name[pos + 1..];
            if !after.is_empty() {
                after.to_string()
            } else {
                raw_name
            }
        } else {
            raw_name
        }
    } else {
        raw_name
    };

    let friendly_name = info
        .get_property_val_str("fn")
        .or_else(|| info.get_property_val_str("n"))
        .or_else(|| info.get_property_val_str("am"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| name.clone());

    let host = pick_best_address(info.get_addresses());

    let port = info.get_port();
    let port = if port > 0 { port } else { default_port };

    let dev_id = format!("{}-{}-{}", output_type, host, port);

    let mut device = DiscoveredDevice::new(dev_id, friendly_name, output_type, host, port);

    // Extract capabilities from TXT records
    let mut caps = HashMap::new();
    if let Some(model) = info.get_property_val_str("md") {
        device.model = Some(model.to_string());
        caps.insert(
            "model".to_string(),
            serde_json::Value::String(model.to_string()),
        );
    }
    if let Some(manufacturer) = info.get_property_val_str("manufacturer") {
        device.manufacturer = Some(manufacturer.to_string());
    }
    if let Some(mac) = info
        .get_property_val_str("deviceid")
        .or_else(|| info.get_property_val_str("id"))
    {
        device.mac_address = Some(mac.to_string());
    }

    // AirPlay version detection
    if output_type == OutputType::Airplay {
        let version = if info.get_property_val_str("features").is_some() {
            "2"
        } else {
            "1"
        };
        device.airplay_version = Some(version.to_string());
        caps.insert("airplay".to_string(), serde_json::Value::Bool(true));
        caps.insert(
            "airplay_version".to_string(),
            serde_json::Value::String(version.to_string()),
        );
    }

    // BluOS capabilities
    if output_type == OutputType::Bluos {
        caps.insert("bluos".to_string(), serde_json::Value::Bool(true));
    }

    // Chromecast model
    if output_type == OutputType::Chromecast {
        caps.insert("chromecast".to_string(), serde_json::Value::Bool(true));
    }

    // Tune peer info
    if output_type == OutputType::Local {
        if let Some(version) = info.get_property_val_str("version") {
            caps.insert(
                "version".to_string(),
                serde_json::Value::String(version.to_string()),
            );
        }
        if let Some(tracks) = info.get_property_val_str("tracks") {
            caps.insert(
                "tracks".to_string(),
                serde_json::Value::String(tracks.to_string()),
            );
        }
    }

    // OAAT endpoint capabilities
    if output_type == OutputType::Oaat {
        if let Some(name_txt) = info.get_property_val_str("name") {
            device.name = name_txt.to_string();
        }
        if let Some(id) = info.get_property_val_str("id") {
            device.id = format!("oaat:{id}");
            device.mac_address = Some(id.to_string());
        }
        if let Some(cap_str) = info.get_property_val_str("caps") {
            caps.insert(
                "caps".into(),
                serde_json::Value::String(cap_str.to_string()),
            );
        }
        if let Some(ch) = info.get_property_val_str("ch") {
            caps.insert("channels".into(), serde_json::Value::String(ch.to_string()));
        }
        if let Some(vendor) = info.get_property_val_str("vendor") {
            device.manufacturer = Some(vendor.to_string());
        }
        if let Some(model) = info.get_property_val_str("model") {
            device.model = Some(model.to_string());
        }
        if let Some(fw) = info.get_property_val_str("fw") {
            caps.insert("firmware".into(), serde_json::Value::String(fw.to_string()));
        }
        caps.insert("oaat".into(), serde_json::Value::Bool(true));
        if let Some(ip) = info.get_property_val_str("ip") {
            device.host = ip.to_string();
        }
    }

    device.capabilities = caps;
    device
}

fn pick_best_address(addrs: &std::collections::HashSet<mdns_sd::ScopedIp>) -> String {
    let local_prefix = detect_local_subnet();
    let mut ipv4_same_subnet: Option<String> = None;
    let mut ipv4_private: Option<String> = None;
    let mut ipv4_any: Option<String> = None;

    for addr in addrs {
        let ip = addr.to_ip_addr();
        if let std::net::IpAddr::V4(v4) = ip {
            let s = v4.to_string();
            if ipv4_any.is_none() {
                ipv4_any = Some(s.clone());
            }
            let octets = v4.octets();
            let is_private = octets[0] == 192
                || octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]));
            if is_private && ipv4_private.is_none() {
                ipv4_private = Some(s.clone());
            }
            if let Some(ref prefix) = local_prefix {
                if s.starts_with(prefix) && ipv4_same_subnet.is_none() {
                    ipv4_same_subnet = Some(s);
                }
            }
        }
    }

    ipv4_same_subnet
        .or(ipv4_private)
        .or(ipv4_any)
        .unwrap_or_else(|| {
            addrs
                .iter()
                .next()
                .map(|a| a.to_ip_addr().to_string())
                .unwrap_or_default()
        })
}

fn detect_local_subnet() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:53").ok()?;
    let addr = sock.local_addr().ok()?;
    if let std::net::IpAddr::V4(v4) = addr.ip() {
        let o = v4.octets();
        Some(format!("{}.{}.{}.", o[0], o[1], o[2]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_constants_end_with_local() {
        assert!(AIRPLAY_SERVICE.ends_with(".local."));
        assert!(BLUOS_SERVICE.ends_with(".local."));
        assert!(CHROMECAST_SERVICE.ends_with(".local."));
        assert!(SQUEEZEBOX_SERVICE.ends_with(".local."));
        assert!(TUNE_SERVICE.ends_with(".local."));
    }

    #[test]
    fn raop_name_strips_mac_prefix() {
        let raw = "800A805D4DEE@DMP-A8";
        let name = if let Some(pos) = raw.find('@') {
            let after = &raw[pos + 1..];
            if !after.is_empty() {
                after.to_string()
            } else {
                raw.to_string()
            }
        } else {
            raw.to_string()
        };
        assert_eq!(name, "DMP-A8");
    }

    #[test]
    fn non_raop_name_unchanged() {
        let raw = "Mac Studio";
        let name = if let Some(pos) = raw.find('@') {
            let after = &raw[pos + 1..];
            if !after.is_empty() {
                after.to_string()
            } else {
                raw.to_string()
            }
        } else {
            raw.to_string()
        };
        assert_eq!(name, "Mac Studio");
    }
}
