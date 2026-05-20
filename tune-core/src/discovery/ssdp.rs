use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::device::{DiscoveredDevice, OutputType};
use super::xml_parser::fetch_device_description;

const SSDP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const PERIODIC_RESCAN_INTERVAL: Duration = Duration::from_secs(300);
const MISS_GRACE_CYCLES: u32 = 3;
const UNICAST_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(30);

pub const MEDIA_RENDERER_URN: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const MEDIA_SERVER_URN: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const SSDP_ALL: &str = "ssdp:all";

const OPENHOME_SEARCH_TARGETS: &[&str] = &[
    "urn:av-openhome-org:service:Product:1",
    "urn:av-openhome-org:service:Playlist:1",
    "urn:linn-co-uk:device:Source:1",
];

#[derive(Debug, Clone)]
pub enum SsdpEvent {
    DeviceDiscovered(Box<DiscoveredDevice>),
    DeviceLost(String),
}

#[derive(Debug)]
struct SsdpResponse {
    location: String,
    usn: String,
    _server: Option<String>,
    _st: Option<String>,
}

pub struct SsdpScanner {
    state: Arc<Mutex<ScannerState>>,
    search_targets: Vec<String>,
    event_tx: mpsc::Sender<SsdpEvent>,
    task: Option<tokio::task::JoinHandle<()>>,
}

struct ScannerState {
    devices: HashMap<String, DiscoveredDevice>,
    known_locations: HashMap<String, String>,
    miss_count: HashMap<String, u32>,
    create_failures: HashMap<String, u32>,
    initial_scan_done: bool,
    last_periodic_rescan: Instant,
}

impl ScannerState {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            known_locations: HashMap::new(),
            miss_count: HashMap::new(),
            create_failures: HashMap::new(),
            initial_scan_done: false,
            last_periodic_rescan: Instant::now(),
        }
    }
}

impl SsdpScanner {
    pub fn new(event_tx: mpsc::Sender<SsdpEvent>) -> Self {
        let mut targets: Vec<String> = vec![MEDIA_RENDERER_URN.to_string()];
        targets.extend(OPENHOME_SEARCH_TARGETS.iter().map(|s| s.to_string()));
        targets.push(SSDP_ALL.to_string());

        Self {
            state: Arc::new(Mutex::new(ScannerState::new())),
            search_targets: targets,
            event_tx,
            task: None,
        }
    }

    pub fn with_targets(mut self, targets: Vec<String>) -> Self {
        self.search_targets = targets;
        self
    }

    pub async fn start(&mut self) {
        let state = self.state.clone();
        let targets = self.search_targets.clone();
        let event_tx = self.event_tx.clone();

        let task = tokio::spawn(async move {
            scan_loop(state, targets, event_tx).await;
        });
        self.task = Some(task);
        info!("ssdp_scanner_started");
    }

    pub async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
        info!("ssdp_scanner_stopped");
    }

    pub async fn rescan(&self) -> Vec<DiscoveredDevice> {
        let responses = search_all(&self.search_targets).await;
        process_responses(&self.state, &self.event_tx, responses).await;
        let state = self.state.lock().await;
        state.devices.values().cloned().collect()
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

async fn scan_loop(
    state: Arc<Mutex<ScannerState>>,
    targets: Vec<String>,
    event_tx: mpsc::Sender<SsdpEvent>,
) {
    loop {
        let responses = search_all(&targets).await;
        process_responses(&state, &event_tx, responses).await;

        {
            let mut st = state.lock().await;
            if !st.initial_scan_done {
                st.initial_scan_done = true;
                if st.devices.is_empty() {
                    drop(st);
                    info!("ssdp_startup_retry: no devices found, retrying in 30s");
                    tokio::time::sleep(STARTUP_RETRY_DELAY).await;
                    let responses = search_all(&targets).await;
                    process_responses(&state, &event_tx, responses).await;
                } else {
                    drop(st);
                }
            } else {
                if st.last_periodic_rescan.elapsed() >= PERIODIC_RESCAN_INTERVAL {
                    info!(devices = st.devices.len(), "ssdp_periodic_rescan");
                    st.last_periodic_rescan = Instant::now();
                }
                drop(st);
            }
        }

        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}

async fn search_all(targets: &[String]) -> Vec<SsdpResponse> {
    let mut all_responses = Vec::new();

    for target in targets {
        match send_msearch(target).await {
            Ok(responses) => all_responses.extend(responses),
            Err(e) => debug!(target, error = %e, "msearch_failed"),
        }
    }

    // Windows multi-NIC fallback: retry with 0.0.0.0
    if all_responses.is_empty() && cfg!(target_os = "windows") {
        debug!("ssdp_windows_fallback_0000");
        for target in targets {
            if let Ok(responses) = send_msearch_from(target, Ipv4Addr::UNSPECIFIED).await {
                all_responses.extend(responses);
            }
        }
    }

    all_responses
}

async fn send_msearch(target: &str) -> Result<Vec<SsdpResponse>, String> {
    let local_ip = get_local_ip().unwrap_or(Ipv4Addr::UNSPECIFIED);
    send_msearch_from(target, local_ip).await
}

async fn send_msearch_from(target: &str, bind_addr: Ipv4Addr) -> Result<Vec<SsdpResponse>, String> {
    let socket = UdpSocket::bind(SocketAddrV4::new(bind_addr, 0))
        .await
        .map_err(|e| format!("bind: {e}"))?;

    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: 5\r\n\
         ST: {target}\r\n\
         \r\n"
    );

    let dest = SocketAddr::from((SSDP_MULTICAST_ADDR, SSDP_PORT));
    socket
        .send_to(msg.as_bytes(), dest)
        .await
        .map_err(|e| format!("send: {e}"))?;

    let mut responses = Vec::new();
    let mut buf = [0u8; 4096];

    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _addr))) => {
                if let Some(resp) = parse_ssdp_response(&buf[..len]) {
                    responses.push(resp);
                }
            }
            Ok(Err(e)) => {
                debug!(error = %e, "ssdp_recv_error");
                break;
            }
            Err(_) => break,
        }
    }

    Ok(responses)
}

fn parse_ssdp_response(data: &[u8]) -> Option<SsdpResponse> {
    let text = std::str::from_utf8(data).ok()?;

    let mut location = None;
    let mut usn = None;
    let mut server = None;
    let mut st = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("LOCATION:").or_else(|| line.strip_prefix("Location:")) {
            location = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("USN:").or_else(|| line.strip_prefix("Usn:")) {
            usn = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("SERVER:").or_else(|| line.strip_prefix("Server:")) {
            server = Some(val.trim().to_string());
        } else if let Some(val) = line.strip_prefix("ST:").or_else(|| line.strip_prefix("St:")) {
            st = Some(val.trim().to_string());
        } else {
            let lower = line.to_lowercase();
            if lower.starts_with("location:") {
                location = Some(line[9..].trim().to_string());
            } else if lower.starts_with("usn:") {
                usn = Some(line[4..].trim().to_string());
            } else if lower.starts_with("server:") {
                server = Some(line[7..].trim().to_string());
            } else if lower.starts_with("st:") {
                st = Some(line[3..].trim().to_string());
            }
        }
    }

    Some(SsdpResponse {
        location: location?,
        usn: usn.unwrap_or_default(),
        _server: server,
        _st: st,
    })
}

fn device_id_from_usn(usn: &str) -> String {
    if let Some(uuid_part) = usn.split("::").next() {
        uuid_part.trim().to_string()
    } else {
        usn.to_string()
    }
}

fn host_from_location(location: &str) -> Option<String> {
    let after_scheme = location.strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;
    Some(host_port.split(':').next()?.to_string())
}

fn port_from_location(location: &str) -> u16 {
    let after_scheme = location.strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
        .unwrap_or(location);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    host_port.split(':').nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(80)
}

async fn process_responses(
    state: &Arc<Mutex<ScannerState>>,
    event_tx: &mpsc::Sender<SsdpEvent>,
    responses: Vec<SsdpResponse>,
) {
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut new_devices: Vec<(String, SsdpResponse)> = Vec::new();

    // Dedup by location
    let mut seen_locations: std::collections::HashSet<String> = std::collections::HashSet::new();
    for resp in responses {
        if seen_locations.contains(&resp.location) {
            continue;
        }
        seen_locations.insert(resp.location.clone());
        let dev_id = device_id_from_usn(&resp.usn);
        seen_ids.insert(dev_id.clone());

        let st = state.lock().await;
        let known = st.known_locations.contains_key(&dev_id);
        drop(st);

        if !known {
            new_devices.push((dev_id, resp));
        } else {
            let mut st = state.lock().await;
            st.miss_count.remove(&dev_id);
        }
    }

    // Fetch device descriptions for new devices
    for (dev_id, resp) in new_devices {
        match fetch_device_description(&resp.location).await {
            Ok(desc) => {
                let host = host_from_location(&resp.location).unwrap_or_default();
                let port = port_from_location(&resp.location);

                let device_type = if desc.is_openhome() {
                    OutputType::Openhome
                } else if desc.is_media_renderer() {
                    OutputType::Dlna
                } else {
                    continue;
                };

                let mut device = DiscoveredDevice::new(
                    dev_id.clone(),
                    desc.friendly_name.clone(),
                    device_type,
                    host,
                    port,
                );
                device.manufacturer = if desc.manufacturer.is_empty() { None } else { Some(desc.manufacturer.clone()) };
                device.model = if desc.model_name.is_empty() { None } else { Some(desc.model_name.clone()) };
                device.location = Some(resp.location.clone());

                device.capabilities.insert(
                    "service_urls".into(),
                    serde_json::to_value(desc.service_urls()).unwrap_or_default(),
                );
                if desc.is_openhome() {
                    device.capabilities.insert("openhome".into(), serde_json::Value::Bool(true));
                }

                let mut st = state.lock().await;
                st.known_locations.insert(dev_id.clone(), resp.location);
                st.miss_count.remove(&dev_id);
                st.create_failures.remove(&dev_id);
                st.devices.insert(dev_id.clone(), device.clone());
                drop(st);

                info!(id = %dev_id, name = %device.name, "ssdp_device_discovered");
                let _ = event_tx.send(SsdpEvent::DeviceDiscovered(Box::new(device))).await;
            }
            Err(e) => {
                let mut st = state.lock().await;
                let count = st.create_failures.entry(dev_id.clone()).or_insert(0);
                *count += 1;
                if *count <= 3 {
                    warn!(id = %dev_id, error = %e, "ssdp_device_create_failed");
                } else {
                    debug!(id = %dev_id, error = %e, "ssdp_device_create_failed");
                }
            }
        }
    }

    // Grace period: check for lost devices
    let mut lost_ids = Vec::new();
    {
        let mut st = state.lock().await;
        let all_known: Vec<String> = st.devices.keys().cloned().collect();
        for dev_id in all_known {
            if !seen_ids.contains(&dev_id) {
                let count = st.miss_count.entry(dev_id.clone()).or_insert(0);
                *count += 1;
                if *count >= MISS_GRACE_CYCLES {
                    lost_ids.push(dev_id);
                }
            }
        }
    }

    // Unicast probe before declaring lost
    for dev_id in lost_ids {
        let probe_ok = unicast_probe(state, &dev_id).await;
        if probe_ok {
            let mut st = state.lock().await;
            st.miss_count.remove(&dev_id);
            debug!(id = %dev_id, "ssdp_unicast_probe_ok");
        } else {
            let mut st = state.lock().await;
            if let Some(mut device) = st.devices.remove(&dev_id) {
                device.available = false;
                st.miss_count.remove(&dev_id);
                st.known_locations.remove(&dev_id);
                info!(id = %dev_id, name = %device.name, "ssdp_device_lost");
                drop(st);
                let _ = event_tx.send(SsdpEvent::DeviceLost(dev_id)).await;
            }
        }
    }
}

async fn unicast_probe(state: &Arc<Mutex<ScannerState>>, dev_id: &str) -> bool {
    let location = {
        let st = state.lock().await;
        st.known_locations.get(dev_id).cloned()
    };

    let Some(location) = location else {
        return false;
    };

    let client = reqwest::Client::builder()
        .timeout(UNICAST_PROBE_TIMEOUT)
        .build();

    let Ok(client) = client else {
        return false;
    };

    match client.get(&location).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

pub fn get_local_ip() -> Option<Ipv4Addr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) => Some(*addr.ip()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_headers() {
        let data = b"HTTP/1.1 200 OK\r\n\
            LOCATION: http://192.168.1.50:1400/xml/device_description.xml\r\n\
            USN: uuid:RINCON_12345::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
            SERVER: Linux UPnP/1.0 Sonos/68.2\r\n\
            ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
            \r\n";

        let resp = parse_ssdp_response(data).unwrap();
        assert_eq!(resp.location, "http://192.168.1.50:1400/xml/device_description.xml");
        assert!(resp.usn.contains("RINCON_12345"));
        assert!(resp._server.unwrap().contains("Sonos"));
    }

    #[test]
    fn device_id_extraction() {
        assert_eq!(
            device_id_from_usn("uuid:12345::urn:schemas-upnp-org:device:MediaRenderer:1"),
            "uuid:12345"
        );
        assert_eq!(device_id_from_usn("uuid:simple"), "uuid:simple");
    }

    #[test]
    fn host_port_extraction() {
        let loc = "http://192.168.1.50:1400/xml/desc.xml";
        assert_eq!(host_from_location(loc), Some("192.168.1.50".into()));
        assert_eq!(port_from_location(loc), 1400);

        let loc2 = "http://10.0.0.1/desc.xml";
        assert_eq!(host_from_location(loc2), Some("10.0.0.1".into()));
        assert_eq!(port_from_location(loc2), 80);
    }

    #[test]
    fn local_ip_detection() {
        let ip = get_local_ip();
        if let Some(ip) = ip {
            assert!(!ip.is_loopback());
            println!("Local IP: {ip}");
        }
    }
}
