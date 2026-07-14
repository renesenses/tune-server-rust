use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::device::{DiscoveredDevice, OutputType};
use super::xml_parser::fetch_device_description;

const SSDP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
const SCAN_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(120);
const PERIODIC_RESCAN_INTERVAL: Duration = Duration::from_secs(300);
const MISS_GRACE_CYCLES: u32 = 3;
const STARTUP_RETRY_DELAY: Duration = Duration::from_secs(30);

pub const MEDIA_RENDERER_URN: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
pub const MEDIA_RENDERER_URN_V2: &str = "urn:schemas-upnp-org:device:MediaRenderer:2";
pub const MEDIA_SERVER_URN: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const SSDP_ALL: &str = "ssdp:all";

#[derive(Debug, Clone)]
pub enum SsdpEvent {
    DeviceDiscovered(Box<DiscoveredDevice>),
    DeviceLost(String),
    MediaServerDiscovered(MediaServerInfo),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MediaServerInfo {
    pub id: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub location: String,
    pub content_directory_url: String,
    pub host: String,
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
        let targets: Vec<String> = vec![SSDP_ALL.to_string()];

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

        // Passive SSDP listener: some legacy renderers (e.g. Cyrus Stream X)
        // never answer M-SEARCH, they only multicast periodic NOTIFY
        // ssdp:alive announcements. Without this they are invisible to the
        // active scanner above. Best-effort: if port 1900 can't be bound the
        // task just exits and active discovery still works.
        let notify_state = self.state.clone();
        let notify_tx = self.event_tx.clone();
        tokio::spawn(async move {
            notify_listen_loop(notify_state, notify_tx).await;
        });

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

        let interval = {
            let st = state.lock().await;
            if st.initial_scan_done && !st.devices.is_empty() {
                IDLE_SCAN_INTERVAL
            } else {
                SCAN_INTERVAL
            }
        };
        tokio::time::sleep(interval).await;
    }
}

/// Passively listen for unsolicited SSDP `NOTIFY` announcements on the
/// multicast group and feed `ssdp:alive` advertisements into the same
/// processing path as active M-SEARCH replies. This is what makes legacy
/// renderers that ignore M-SEARCH (but still announce themselves) discoverable.
async fn notify_listen_loop(state: Arc<Mutex<ScannerState>>, event_tx: mpsc::Sender<SsdpEvent>) {
    let socket = match bind_notify_socket() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "ssdp_notify_listener_disabled");
            return;
        }
    };
    info!("ssdp_notify_listener_started");

    let mut buf = [0u8; 4096];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                let data = &buf[..len];
                // Only react to NOTIFY datagrams (ignore our own and others'
                // M-SEARCH requests, and M-SEARCH replies handled elsewhere).
                let head = String::from_utf8_lossy(&data[..len.min(256)]);
                if !head.starts_with("NOTIFY") {
                    continue;
                }
                let is_byebye = head.contains("ssdp:byebye");
                if is_byebye {
                    if let Some(resp) = parse_ssdp_response(data) {
                        let dev_id = device_id_from_usn(&resp.usn);
                        let _ = event_tx.send(SsdpEvent::DeviceLost(dev_id)).await;
                    } else if let Some(usn) = usn_from_raw(data) {
                        let _ = event_tx
                            .send(SsdpEvent::DeviceLost(device_id_from_usn(&usn)))
                            .await;
                    }
                    continue;
                }
                // ssdp:alive (or update): reuse the M-SEARCH processing path.
                // process_responses dedups by location/USN, so repeated
                // announcements for an already-known device are cheap.
                if let Some(resp) = parse_ssdp_response(data) {
                    process_responses(&state, &event_tx, vec![resp]).await;
                } else {
                    debug!(from = %addr, bytes = len, "ssdp_notify_unparseable");
                }
            }
            Err(e) => {
                debug!(error = %e, "ssdp_notify_recv_error");
                // Transient errors shouldn't spin the loop hot.
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Bind a UDP socket to the SSDP multicast port for passive listening.
/// Uses SO_REUSEADDR/SO_REUSEPORT so it can coexist with other SSDP users on
/// the host (other apps, our own UPnP server), and joins the multicast group
/// on every real IPv4 interface for multi-NIC / VPN setups.
fn bind_notify_socket() -> Result<UdpSocket, String> {
    let sock2 = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .map_err(|e| format!("socket2 new: {e}"))?;
    sock2.set_reuse_address(true).ok();
    #[cfg(unix)]
    sock2.set_reuse_port(true).ok();
    sock2
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            SSDP_PORT,
        )))
        .map_err(|e| format!("bind 0.0.0.0:{SSDP_PORT}: {e}"))?;

    // Join the multicast group on each real interface (and the default).
    let mut joined = false;
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = iface.ip()
                && sock2.join_multicast_v4(&SSDP_MULTICAST_ADDR, &ip).is_ok()
            {
                joined = true;
            }
        }
    }
    if !joined {
        sock2
            .join_multicast_v4(&SSDP_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)
            .map_err(|e| format!("join_multicast: {e}"))?;
    }

    sock2
        .set_nonblocking(true)
        .map_err(|e| format!("nonblock: {e}"))?;
    UdpSocket::from_std(std::net::UdpSocket::from(sock2)).map_err(|e| format!("from_std: {e}"))
}

/// Extract the USN header from a raw SSDP datagram even when LOCATION is
/// absent (ssdp:byebye carries no LOCATION).
fn usn_from_raw(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line
            .strip_prefix("USN:")
            .or_else(|| line.strip_prefix("Usn:"))
            .or_else(|| {
                if line.to_lowercase().starts_with("usn:") {
                    Some(&line[4..])
                } else {
                    None
                }
            })
        {
            return Some(val.trim().to_string());
        }
    }
    None
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
    let mut all_responses = Vec::new();
    let mut tried = std::collections::HashSet::new();

    // Enumerate all real network interfaces (works in Docker macvlan, VPN, multi-NIC)
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = iface.ip()
                && !tried.contains(&ip)
            {
                tried.insert(ip);
                debug!(interface = %iface.name, ip = %ip, "ssdp_probing_interface");
                if let Ok(resps) = send_msearch_from(target, ip).await {
                    all_responses.extend(resps);
                }
            }
        }
    }

    // Fallback: also try 0.0.0.0 if no interface found or no responses
    if all_responses.is_empty()
        && let Ok(resps) = send_msearch_from(target, Ipv4Addr::UNSPECIFIED).await
    {
        all_responses.extend(resps);
    }

    Ok(all_responses)
}

async fn send_msearch_from(target: &str, bind_ip: Ipv4Addr) -> Result<Vec<SsdpResponse>, String> {
    // Use socket2 with explicit multicast interface binding for VPN compat
    let sock2 = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .map_err(|e| format!("socket2 new: {e}"))?;
    sock2.set_reuse_address(true).ok();
    // Bind to the specific LAN IP so responses come back on the right interface
    sock2
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(bind_ip, 0)))
        .map_err(|e| format!("bind {bind_ip}: {e}"))?;
    sock2
        .set_multicast_if_v4(&bind_ip)
        .map_err(|e| format!("multicast_if: {e}"))?;
    sock2.join_multicast_v4(&SSDP_MULTICAST_ADDR, &bind_ip).ok();
    sock2.set_multicast_ttl_v4(4).ok();
    sock2
        .set_nonblocking(true)
        .map_err(|e| format!("nonblock: {e}"))?;
    let socket = UdpSocket::from_std(std::net::UdpSocket::from(sock2))
        .map_err(|e| format!("from_std: {e}"))?;

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
    let mut recv_count: u32 = 0;

    let deadline = tokio::time::Instant::now() + SEARCH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, addr))) => {
                recv_count += 1;
                if let Some(resp) = parse_ssdp_response(&buf[..len]) {
                    responses.push(resp);
                } else {
                    debug!(from = %addr, bytes = len, "ssdp_unparseable_response");
                }
            }
            Ok(Err(e)) => {
                debug!(error = %e, "ssdp_recv_error");
                continue;
            }
            Err(_) => break,
        }
    }
    debug!(bind = %bind_ip, target, recv_count, parsed = responses.len(), "ssdp_search_done");

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
        if let Some(val) = line
            .strip_prefix("LOCATION:")
            .or_else(|| line.strip_prefix("Location:"))
        {
            location = Some(val.trim().to_string());
        } else if let Some(val) = line
            .strip_prefix("USN:")
            .or_else(|| line.strip_prefix("Usn:"))
        {
            usn = Some(val.trim().to_string());
        } else if let Some(val) = line
            .strip_prefix("SERVER:")
            .or_else(|| line.strip_prefix("Server:"))
        {
            server = Some(val.trim().to_string());
        } else if let Some(val) = line
            .strip_prefix("ST:")
            .or_else(|| line.strip_prefix("St:"))
        {
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
    let after_scheme = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))?;
    let host_port = after_scheme.split('/').next()?;
    Some(host_port.split(':').next()?.to_string())
}

fn base_url_from_location(location: &str) -> String {
    let scheme = if location.starts_with("https://") {
        "https://"
    } else {
        "http://"
    };
    let after_scheme = location.strip_prefix(scheme).unwrap_or(location);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    format!("{scheme}{host_port}")
}

fn port_from_location(location: &str) -> u16 {
    let after_scheme = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))
        .unwrap_or(location);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    host_port
        .split(':')
        .nth(1)
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

        if let Some(host_str) = host_from_location(&resp.location) {
            if let Ok(ip) = host_str.parse::<std::net::Ipv4Addr>() {
                if is_virtual_ip(ip) {
                    debug!(
                        location = %resp.location,
                        ip = %ip,
                        "ssdp_response_rejected_virtual_ip_in_location"
                    );
                    continue;
                }
            }
        }

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
                } else if desc.has_av_transport() {
                    // Non-standard deviceType but supports AVTransport (WiiM, foobar2000 foo_upnp, etc.)
                    debug!(
                        id = %dev_id,
                        name = %desc.friendly_name,
                        device_type = %desc.device_type,
                        "ssdp_non_standard_renderer_accepted"
                    );
                    OutputType::Dlna
                } else if desc.is_media_server() {
                    let cd_url = desc
                        .services
                        .iter()
                        .find(|s| s.service_type.contains("ContentDirectory"))
                        .map(|s| s.control_url.clone())
                        .unwrap_or_default();
                    if !cd_url.is_empty() {
                        let host = host_from_location(&resp.location).unwrap_or_default();
                        let base = base_url_from_location(&resp.location);
                        let full_cd_url = if cd_url.starts_with("http") {
                            cd_url
                        } else {
                            format!("{base}{cd_url}")
                        };
                        let ms = MediaServerInfo {
                            id: dev_id.clone(),
                            name: desc.friendly_name.clone(),
                            manufacturer: desc.manufacturer.clone(),
                            model: desc.model_name.clone(),
                            location: resp.location.clone(),
                            content_directory_url: full_cd_url,
                            host,
                        };
                        // Record the media server as known so later SSDP cycles
                        // skip it (see the `!known` gate above). Renderers are
                        // recorded the same way further down; media servers were
                        // omitted, so every ~2 min cycle re-fetched their
                        // description and re-logged this INFO line — dozens of
                        // duplicate `ssdp_media_server_discovered` entries that
                        // drowned the playback traces in tester logs and made
                        // DLNA issues undiagnosable (#954).
                        state
                            .lock()
                            .await
                            .known_locations
                            .insert(dev_id.clone(), resp.location.clone());
                        info!(
                            id = %dev_id,
                            name = %ms.name,
                            cd_url = %ms.content_directory_url,
                            "ssdp_media_server_discovered"
                        );
                        let _ = event_tx.send(SsdpEvent::MediaServerDiscovered(ms)).await;
                    }
                    continue;
                } else {
                    debug!(
                        id = %dev_id,
                        name = %desc.friendly_name,
                        device_type = %desc.device_type,
                        "ssdp_device_skipped"
                    );
                    continue;
                };

                let mut device = DiscoveredDevice::new(
                    dev_id.clone(),
                    desc.friendly_name.clone(),
                    device_type,
                    host,
                    port,
                );
                device.manufacturer = if desc.manufacturer.is_empty() {
                    None
                } else {
                    Some(desc.manufacturer.clone())
                };
                device.model = if desc.model_name.is_empty() {
                    None
                } else {
                    Some(desc.model_name.clone())
                };
                device.location = Some(resp.location.clone());

                device.capabilities.insert(
                    "service_urls".into(),
                    serde_json::to_value(desc.service_urls()).unwrap_or_default(),
                );
                device.capabilities.insert(
                    "event_sub_urls".into(),
                    serde_json::to_value(desc.event_sub_urls()).unwrap_or_default(),
                );
                if desc.is_openhome() {
                    device
                        .capabilities
                        .insert("openhome".into(), serde_json::Value::Bool(true));
                }

                let mut st = state.lock().await;
                st.known_locations.insert(dev_id.clone(), resp.location);
                st.miss_count.remove(&dev_id);
                st.create_failures.remove(&dev_id);
                st.devices.insert(dev_id.clone(), device.clone());
                drop(st);

                info!(id = %dev_id, name = %device.name, "ssdp_device_discovered");
                let _ = event_tx
                    .send(SsdpEvent::DeviceDiscovered(Box::new(device)))
                    .await;
            }
            Err(e) => {
                let failure_count = {
                    let mut st = state.lock().await;
                    let count = st.create_failures.entry(dev_id.clone()).or_insert(0);
                    *count += 1;
                    *count
                };

                // Try MinimalDMR probe on first failure
                if failure_count == 1 {
                    let host = host_from_location(&resp.location).unwrap_or_default();
                    let port = port_from_location(&resp.location);
                    let base_url = format!("http://{host}:{port}");
                    let fallback_name = format!("Renderer ({host})");
                    if let Some(probe) = super::minimal_dmr::probe_minimal_dmr(
                        &base_url,
                        Some(&resp.location),
                        &fallback_name,
                    )
                    .await
                    {
                        let mut device = DiscoveredDevice::new(
                            dev_id.clone(),
                            probe.name.clone(),
                            OutputType::Dlna,
                            host,
                            port,
                        );
                        device.location = Some(resp.location.clone());
                        let mut svc_urls = std::collections::HashMap::new();
                        svc_urls.insert("AVTransport".to_string(), probe.av_transport_url.clone());
                        if let Some(ref rc) = probe.rendering_control_url {
                            svc_urls.insert("RenderingControl".to_string(), rc.clone());
                        }
                        device.capabilities.insert(
                            "service_urls".into(),
                            serde_json::to_value(&svc_urls).unwrap_or_default(),
                        );
                        device
                            .capabilities
                            .insert("minimal_dmr".into(), serde_json::Value::Bool(true));

                        let mut st = state.lock().await;
                        st.known_locations.insert(dev_id.clone(), resp.location);
                        st.miss_count.remove(&dev_id);
                        st.create_failures.remove(&dev_id);
                        st.devices.insert(dev_id.clone(), device.clone());
                        drop(st);

                        info!(id = %dev_id, name = %probe.name, "ssdp_minimal_dmr_discovered");
                        let _ = event_tx
                            .send(SsdpEvent::DeviceDiscovered(Box::new(device)))
                            .await;
                        continue;
                    }
                }

                if failure_count <= 3 {
                    warn!(id = %dev_id, error = %e, "ssdp_device_create_failed");
                }
                let mut st = state.lock().await;
                if st.create_failures.len() > 200 {
                    st.create_failures.retain(|_, c| *c < 50);
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

    let client = crate::http::client::shared();

    match client.get(&location).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

pub fn get_local_ip() -> Option<Ipv4Addr> {
    // --- Step 1: UDP connect probe (follows the OS default route → real LAN) ---
    let probe_ip = udp_probe_ip();
    if let Some(ip) = probe_ip {
        if is_virtual_ip(ip) || ip_on_virtual_interface(ip) {
            debug!(ip = %ip, "udp_probe_returned_virtual_ip_skipping");
        } else {
            let o = ip.octets();
            // If probe returned a 10.x.x.x address, check whether a 192.168.x.x
            // interface exists — if so, prefer the LAN address since 10.x.x.x is
            // often a VPN tunnel that DLNA renderers cannot reach (B-06).
            let prefer_interface_enum = o[0] == 10 && has_192_168_interface();
            if prefer_interface_enum {
                debug!(
                    ip = %ip,
                    "udp_probe_returned_10x_but_192168_available_deferring"
                );
            } else {
                debug!(ip = %ip, method = "udp_probe", "local_ip_detected");
                return Some(ip);
            }
        }
    }

    // --- Step 2: enumerate interfaces, skip virtual adapters ---
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        // Score each candidate: higher = more preferred
        let mut candidates: Vec<(Ipv4Addr, u8)> = Vec::new();
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = iface.ip() {
                if is_virtual_interface(&iface.name, ip) {
                    debug!(name = %iface.name, ip = %ip, "skipping_virtual_interface");
                    continue;
                }
                let o = ip.octets();
                let score = if o[0] == 192 && o[1] == 168 {
                    // 192.168.x.x — typical home LAN, highest priority
                    30
                } else if o[0] == 10 {
                    // 10.x.x.x — could be real LAN or VPN, medium priority
                    20
                } else if o[0] == 172 && o[1] >= 16 && o[1] <= 31 {
                    // 172.16-31.x.x — less common for home LANs
                    10
                } else {
                    5
                };
                candidates.push((ip, score));
            }
        }
        // Pick highest-scoring candidate
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some((ip, _)) = candidates.first() {
            debug!(ip = %ip, method = "interface_enum", "local_ip_detected");
            return Some(*ip);
        }
    }

    // --- Step 3: fall back to UDP probe even if it's virtual (better than nothing) ---
    if let Some(ip) = probe_ip {
        warn!(ip = %ip, "local_ip_fallback_to_virtual");
        return Some(ip);
    }

    warn!("local_ip_detection_failed");
    None
}

/// Returns true if any non-loopback, non-virtual interface has a 192.168.x.x address.
fn has_192_168_interface() -> bool {
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = iface.ip() {
                let o = ip.octets();
                if o[0] == 192 && o[1] == 168 && !is_virtual_interface(&iface.name, ip) {
                    return true;
                }
            }
        }
    }
    false
}

/// Returns true if `target` is bound to a known virtual/VPN interface.
/// Used to reject a udp-probe result that landed on a VPN tunnel (e.g. NordVPN
/// captures the default route, so the probe returns the tunnel IP that LAN
/// renderers cannot reach — Pierre Mack QA: NordLynx 10.5.0.2 advertised instead
/// of the real LAN 10.117.x).
fn ip_on_virtual_interface(target: Ipv4Addr) -> bool {
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if let std::net::IpAddr::V4(ip) = iface.ip() {
                if ip == target {
                    return is_virtual_interface(&iface.name, ip);
                }
            }
        }
    }
    false
}

/// UDP connect probe: the OS picks the interface for the default route.
fn udp_probe_ip() -> Option<Ipv4Addr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(addr) => Some(*addr.ip()),
        _ => None,
    }
}

/// Returns true if the interface name or IP belongs to a known virtual adapter
/// (VirtualBox, Docker, VMware, Hyper-V, libvirt, VPN tunnels, WSL).
fn is_virtual_interface(name: &str, ip: Ipv4Addr) -> bool {
    // Check by interface name (case-insensitive)
    let lower = name.to_lowercase();
    let virtual_name_prefixes = [
        "vbox",       // VirtualBox
        "virtualbox", // VirtualBox (alt)
        "vmnet",      // VMware
        "docker",     // Docker bridge
        "br-",        // Docker custom bridges
        "veth",       // Docker/container veth pairs
        "virbr",      // libvirt/KVM
        "vethernet",  // Hyper-V / WSL
        "tailscale",  // Tailscale VPN
        "nordlynx",   // NordVPN (Windows NordLynx / WireGuard adapter)
        "nordvpn",    // NordVPN (alt adapter name)
        "wg",         // WireGuard
        "wireguard",  // WireGuard (full name)
        "proton",     // ProtonVPN
        "tun",        // VPN tunnel
        "utun",       // macOS VPN tunnel (utun0, utun1, ...)
        "ham",        // Hamachi VPN
        "zt",         // ZeroTier
    ];
    for prefix in &virtual_name_prefixes {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    // Check by well-known virtual IP ranges
    is_virtual_ip(ip)
}

/// Returns true if the IP falls in a well-known virtual adapter subnet.
fn is_virtual_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // Tailscale CGNAT range: 100.64.0.0/10 (100.64.0.0 – 100.127.255.255)
    // DEvir QA B-06: DLNA fails when get_local_ip() returns a Tailscale IP
    // because DLNA renderers on the LAN cannot reach 100.x.x.x addresses.
    if o[0] == 100 && (o[1] & 0xC0) == 64 {
        return true;
    }
    // VirtualBox Host-Only default: 192.168.56.x
    if o[0] == 192 && o[1] == 168 && o[2] == 56 {
        return true;
    }
    // VMware default ranges: 192.168.{52,137,138,139}.x
    if o[0] == 192 && o[1] == 168 && (o[2] == 52 || o[2] == 137 || o[2] == 138 || o[2] == 139) {
        return true;
    }
    // Docker default bridge: 172.17.x.x
    if o[0] == 172 && o[1] == 17 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nordvpn_interface_is_virtual() {
        // NordVPN's NordLynx adapter must be treated as virtual so get_local_ip
        // never advertises its tunnel IP to LAN renderers (Pierre Mack: Klimax
        // got 10.5.0.2 instead of the real LAN 10.117.x).
        assert!(is_virtual_interface("NordLynx", Ipv4Addr::new(10, 5, 0, 2)));
        assert!(is_virtual_interface(
            "NordVPN Tunnel",
            Ipv4Addr::new(10, 5, 0, 2)
        ));
        // A real wired adapter must NOT be flagged, even on a 10.x LAN.
        assert!(!is_virtual_interface(
            "Ethernet",
            Ipv4Addr::new(10, 117, 233, 82)
        ));
        assert!(!is_virtual_interface(
            "Realtek Gaming 2.5GbE Family Controller",
            Ipv4Addr::new(192, 168, 1, 50)
        ));
    }

    #[test]
    fn parse_response_headers() {
        let data = b"HTTP/1.1 200 OK\r\n\
            LOCATION: http://192.168.1.50:1400/xml/device_description.xml\r\n\
            USN: uuid:RINCON_12345::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
            SERVER: Linux UPnP/1.0 Sonos/68.2\r\n\
            ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
            \r\n";

        let resp = parse_ssdp_response(data).unwrap();
        assert_eq!(
            resp.location,
            "http://192.168.1.50:1400/xml/device_description.xml"
        );
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

    #[test]
    fn virtual_ip_detection() {
        // Tailscale CGNAT range: 100.64.0.0/10
        assert!(is_virtual_ip(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(is_virtual_ip(Ipv4Addr::new(100, 100, 50, 2)));
        assert!(is_virtual_ip(Ipv4Addr::new(100, 127, 255, 255)));
        // 100.x outside CGNAT range must NOT be flagged
        assert!(!is_virtual_ip(Ipv4Addr::new(100, 0, 0, 1)));
        assert!(!is_virtual_ip(Ipv4Addr::new(100, 128, 0, 1)));
        // VirtualBox Host-Only default
        assert!(is_virtual_ip(Ipv4Addr::new(192, 168, 56, 1)));
        assert!(is_virtual_ip(Ipv4Addr::new(192, 168, 56, 100)));
        // VMware defaults
        assert!(is_virtual_ip(Ipv4Addr::new(192, 168, 137, 1)));
        assert!(is_virtual_ip(Ipv4Addr::new(192, 168, 52, 1)));
        // Docker bridge
        assert!(is_virtual_ip(Ipv4Addr::new(172, 17, 0, 1)));
        // Real LAN IPs must NOT be flagged
        assert!(!is_virtual_ip(Ipv4Addr::new(192, 168, 1, 100)));
        assert!(!is_virtual_ip(Ipv4Addr::new(192, 168, 0, 1)));
        assert!(!is_virtual_ip(Ipv4Addr::new(10, 0, 0, 50)));
        assert!(!is_virtual_ip(Ipv4Addr::new(172, 16, 0, 1)));
    }

    #[test]
    fn virtual_interface_detection() {
        let real_ip = Ipv4Addr::new(192, 168, 1, 100);
        let vbox_ip = Ipv4Addr::new(192, 168, 56, 1);

        // Virtual adapters by name
        assert!(is_virtual_interface("vboxnet0", real_ip));
        assert!(is_virtual_interface("VirtualBox Host-Only", real_ip));
        assert!(is_virtual_interface("vmnet8", real_ip));
        assert!(is_virtual_interface("docker0", real_ip));
        assert!(is_virtual_interface("br-abc123", real_ip));
        assert!(is_virtual_interface("veth1234", real_ip));
        assert!(is_virtual_interface("virbr0", real_ip));
        assert!(is_virtual_interface("tailscale0", real_ip));
        assert!(is_virtual_interface("wg0", real_ip));
        assert!(is_virtual_interface("tun0", real_ip));
        assert!(is_virtual_interface("utun3", real_ip));
        assert!(is_virtual_interface("zt0", real_ip));

        // Virtual adapter by IP (even with real-looking name)
        assert!(is_virtual_interface("eth1", vbox_ip));

        // Real adapters must NOT be flagged
        assert!(!is_virtual_interface("eth0", real_ip));
        assert!(!is_virtual_interface("en0", real_ip));
        assert!(!is_virtual_interface("enp3s0", real_ip));
        assert!(!is_virtual_interface("wlan0", real_ip));
        assert!(!is_virtual_interface("Wi-Fi", real_ip));
        assert!(!is_virtual_interface("Ethernet", real_ip));
    }
}
