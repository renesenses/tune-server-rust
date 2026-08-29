use std::collections::HashMap;
use std::sync::Arc;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use super::device::{DiscoveredDevice, OutputType};

pub const AIRPLAY_SERVICE: &str = "_raop._tcp.local.";
pub const AIRPLAY2_SERVICE: &str = "_airplay._tcp.local.";
pub const BLUOS_SERVICE: &str = "_musc._tcp.local.";
pub const BLUOS_SERVICE2: &str = "_musp._tcp.local.";
pub const BLUOS_SERVICE3: &str = "_musz._tcp.local.";
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
        // BluOS devices advertise multiple mDNS service types: _musc, _musp,
        // _musz.  Browse all of them to maximise discovery reliability
        // (some devices/networks only respond to a subset).
        for svc in [BLUOS_SERVICE, BLUOS_SERVICE2, BLUOS_SERVICE3] {
            self.configs.push(MdnsServiceConfig {
                service_type: svc.to_string(),
                output_type: OutputType::Bluos,
                default_port: 11000,
            });
        }
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
        // Real OS hostname (not the env-only derivation that collapsed to
        // "tune-server" under systemd, colliding every instance — #1112).
        let hostname = crate::discovery::system_hostname();
        let service_name = format!("Tune ({hostname})");
        let host_label = crate::discovery::mdns_host_label(&hostname);

        let local_ip = crate::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());

        let properties = [("version", version), ("path", "/api/v1")];

        let svc = ServiceInfo::new(
            TUNE_SERVICE,
            &service_name,
            &format!("{host_label}.local."),
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

            // Skip duplicate services for the same host+type (e.g. RAOP vs
            // AirPlay2, or _musc vs _musp vs _musz for BluOS).
            //
            // AirPlay : le verdict se rend sur l'identité (MAC) et plus
            // seulement sur l'adresse. Une même enceinte annonce `_raop` ET
            // `_airplay`, et chaque résolution peut retenir une adresse
            // différente (IPv4 pour l'une, IPv6 pour l'autre) : comparées à
            // l'adresse, ces deux annonces devenaient deux sorties
            // concurrentes dont une seule était jouable — la zone ancrée sur
            // l'adresse IPv6 échouait en « Network is unreachable » là où
            // IPv6 n'est pas routé, Docker en tête (#197, paire Devialet
            // Phantom « SALON »). Et quand l'adresse IPv4 arrive après coup,
            // on RÉPARE l'appareil retenu au lieu de jeter l'annonce.
            if output_type == OutputType::Airplay {
                match verdict_doublon_airplay(st.devices.values(), &device) {
                    DoublonAirplay::Nouveau => {}
                    DoublonAirplay::Ignorer => {
                        debug!(id = %dev_id, name = %device.name, host = %device.host, service = service_type, "mdns_dup_skipped");
                        drop(st);
                        return;
                    }
                    DoublonAirplay::Reprendre { id_retenu } => {
                        if let Some(d) = st.devices.get_mut(&id_retenu) {
                            d.host = device.host.clone();
                            let repris = d.clone();
                            st.service_to_device
                                .insert(info.get_fullname().to_string(), id_retenu.clone());
                            drop(st);
                            info!(
                                id = %id_retenu,
                                name = %repris.name,
                                host = %repris.host,
                                service = service_type,
                                "mdns_airplay_ipv6_repris_en_ipv4"
                            );
                            let _ = event_tx.send(MdnsEvent::DeviceUpdated(repris)).await;
                        }
                        return;
                    }
                }
            } else if output_type == OutputType::Bluos {
                let host_already = st.devices.values().any(|d| {
                    d.device_type == output_type && d.host == device.host && d.id != dev_id
                });
                if host_already {
                    debug!(id = %dev_id, name = %device.name, host = %device.host, service = service_type, "mdns_dup_skipped");
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

    // RAOP names look like "800A805D4DEE@DMP-A8" — keep the MAC as identity…
    let raop_mac = if output_type == OutputType::Airplay {
        mac_depuis_nom_raop(&raw_name)
    } else {
        None
    };

    // …and strip the hex MAC prefix from the display name.
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

    // L'identifiant que l'appareil annonce lui-meme, lu AVANT tout
    // enrichissement et jamais reecrit ensuite (cf. `stable_id`).
    let stable_id = info
        .get_property_val_str("deviceid")
        .or_else(|| info.get_property_val_str("id"))
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());

    let dev_id = device_id_for(output_type, stable_id.as_deref(), &host, port);

    let mut device = DiscoveredDevice::new(dev_id, friendly_name, output_type, host, port);
    device.stable_id = stable_id;

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
    } else if output_type == OutputType::Airplay {
        // Le nom d'instance RAOP porte la MAC de l'appareil
        // (« 800A805D4DEE@DMP-A8 ») : c'est la même identité que le TXT
        // `deviceid` de `_airplay`. La retenir permet au dédoublonnage de
        // reconnaître les deux services comme UN appareil même quand leurs
        // résolutions ont retenu des adresses différentes (#197).
        device.mac_address = raop_mac;
    }

    // AirPlay version detection + features/flags parsing.
    if output_type == OutputType::Airplay {
        let features_raw = info.get_property_val_str("features");
        let version = if features_raw.is_some() { "2" } else { "1" };
        device.airplay_version = Some(version.to_string());
        caps.insert("airplay".to_string(), serde_json::Value::Bool(true));
        caps.insert(
            "airplay_version".to_string(),
            serde_json::Value::String(version.to_string()),
        );

        // Parse the AirPlay `features` bitmask (and `flags`) so callers can tell
        // whether the receiver demands a HomeKit-style pair-setup before it will
        // accept an RTSP session (Apple TV, Samsung/LG TVs, HomePod, ...).
        if let Some(raw) = features_raw {
            if let Some(bits) = parse_airplay_features(raw) {
                caps.insert(
                    "airplay_features".to_string(),
                    serde_json::Value::String(format!("0x{bits:016X}")),
                );
                let needs_pairing = airplay_requires_pairing(bits);
                caps.insert(
                    "airplay_requires_pairing".to_string(),
                    serde_json::Value::Bool(needs_pairing),
                );
            }
        }
        // The `flags` TXT independently signals "PIN required" (bit 9 / 0x200)
        // on many receivers even when features are ambiguous.
        if let Some(flags_raw) = info.get_property_val_str("flags") {
            if let Some(flags) = parse_hex_u64(flags_raw) {
                if flags & AIRPLAY_FLAG_PIN_REQUIRED != 0 {
                    caps.insert(
                        "airplay_requires_pairing".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
            }
        }

        // Groupe AirPlay 2 (paire stéréo, multi-room) : `gid` identifie le
        // groupe, `igl` dit si CET appareil en est le meneur, `gpn` porte le
        // nom public du groupe. Capturé tel quel pour le diagnostic (#197,
        // paire Devialet « SALON ») — aucun comportement n'en dépend encore.
        if let Some(gid) = info.get_property_val_str("gid") {
            caps.insert(
                "airplay_group_id".to_string(),
                serde_json::Value::String(gid.to_string()),
            );
        }
        if let Some(igl) = info.get_property_val_str("igl") {
            caps.insert(
                "airplay_group_leader".to_string(),
                serde_json::Value::Bool(igl.trim() == "1"),
            );
        }
        if let Some(gpn) = info.get_property_val_str("gpn") {
            caps.insert(
                "airplay_group_name".to_string(),
                serde_json::Value::String(gpn.to_string()),
            );
        }
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
    // Normalise whatever landed in mac_address (AirPlay deviceid, opaque
    // Chromecast id → ARP fallback) and derive the brand from the OUI when
    // the TXT record carried no manufacturer.
    super::mac::enrich_identity(&mut device);
    device
}

/// La MAC portée par un nom d'instance RAOP (« 800A805D4DEE@DMP-A8 »),
/// normalisée. `None` si le nom n'en porte pas (pas de `@`, ou un préfixe qui
/// n'est pas une MAC).
fn mac_depuis_nom_raop(raw_name: &str) -> Option<String> {
    let pos = raw_name.find('@')?;
    super::mac::normalize_mac(&raw_name[..pos])
}

/// Verdict du dédoublonnage AirPlay — voir l'appelant dans `handle_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DoublonAirplay {
    /// Aucun appareil connu ne correspond : on l'insère.
    Nouveau,
    /// Un appareil connu correspond et son adresse vaut mieux (ou autant) :
    /// on jette l'annonce.
    Ignorer,
    /// Un appareil connu correspond mais il est ancré sur une adresse IPv6
    /// alors que l'annonce apporte une IPv4 : on répare l'appareil retenu en
    /// place (même identifiant, donc même zone) au lieu de jeter l'annonce.
    /// C'est ce qui sort une zone du « Network is unreachable (os error 101) »
    /// quand IPv6 n'est pas routé (#197).
    Reprendre { id_retenu: String },
}

/// Deux appareils AirPlay sont LE MÊME appareil physique s'ils partagent
/// l'adresse… ou la MAC : `_raop` la porte dans son nom d'instance,
/// `_airplay` dans son TXT `deviceid`, et chaque service peut avoir résolu
/// une adresse différente (IPv4 contre IPv6).
fn meme_appareil_airplay(a: &DiscoveredDevice, b: &DiscoveredDevice) -> bool {
    if a.host == b.host && !a.host.is_empty() {
        return true;
    }
    match (
        a.mac_address.as_deref().and_then(super::mac::normalize_mac),
        b.mac_address.as_deref().and_then(super::mac::normalize_mac),
    ) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn est_ipv6(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_ipv6())
        .unwrap_or(false)
}

fn est_ipv4(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_ipv4())
        .unwrap_or(false)
}

fn verdict_doublon_airplay<'a>(
    existants: impl Iterator<Item = &'a DiscoveredDevice>,
    nouveau: &DiscoveredDevice,
) -> DoublonAirplay {
    let doublon = existants
        .filter(|d| d.device_type == OutputType::Airplay && d.id != nouveau.id)
        .find(|d| meme_appareil_airplay(d, nouveau));
    match doublon {
        None => DoublonAirplay::Nouveau,
        Some(d) if est_ipv6(&d.host) && est_ipv4(&nouveau.host) => DoublonAirplay::Reprendre {
            id_retenu: d.id.clone(),
        },
        Some(_) => DoublonAirplay::Ignorer,
    }
}

/// L'identifiant durable d'un appareil.
///
/// Prefere ce que l'appareil annonce lui-meme ; ne retombe sur l'adresse que
/// lorsqu'il n'annonce rien. C'est tout l'objet de #1528 : un bail DHCP
/// renouvele changeait l'identite de l'appareil, donc dedoublait sa zone et
/// faisait revenir les zones supprimees, puisque tout le cycle de vie d'une
/// zone repose sur cette chaine.
pub fn device_id_for(
    output_type: OutputType,
    stable_id: Option<&str>,
    host: &str,
    port: u16,
) -> String {
    match stable_id {
        Some(id) => format!("{output_type}-{id}"),
        None => legacy_device_id(output_type, host, port),
    }
}

/// L'ancienne forme, derivee de l'adresse.
///
/// Toujours produite pour les appareils qui n'annoncent aucun identifiant, et
/// surtout : c'est sous cette forme que sont enregistrees les zones creees
/// AVANT #1528. La decouverte s'en sert pour les retrouver et les re-ancrer
/// (`discovery_setup`), ce qui evite la migration SQL qui aurait fait perdre
/// toutes les zones d'un coup.
pub fn legacy_device_id(output_type: OutputType, host: &str, port: u16) -> String {
    format!("{output_type}-{host}-{port}")
}

fn pick_best_address(addrs: &std::collections::HashSet<mdns_sd::ScopedIp>) -> String {
    let ips: Vec<std::net::IpAddr> = addrs.iter().map(|a| a.to_ip_addr()).collect();
    choose_address(&ips, detect_local_subnet().as_deref())
}

/// Choisit l'adresse qui servira d'identité à l'appareil.
///
/// Cette fonction **doit rendre le même résultat pour un même jeu d'adresses**,
/// quel que soit l'ordre dans lequel elles arrivent. `device_id` en est dérivé
/// (`{type}-{host}-{port}`) et tout le cycle de vie d'une zone repose dessus :
/// une identité qui change d'un démarrage à l'autre dédouble la zone, et fait
/// revenir celles que l'utilisateur avait supprimées — le garde-fou
/// `is_device_hidden` porte sur l'ancien identifiant et ne reconnaît plus le
/// nouveau (#1528).
///
/// Or l'appelant itère un `HashSet`, dont l'ordre n'est pas déterministe. Un
/// appareil à deux pattes sur le même sous-réseau (Wi-Fi et Ethernet) tombait
/// donc tantôt sur l'une, tantôt sur l'autre, **sans que rien n'ait bougé sur
/// le réseau**. D'où le tri, qui ne coûte rien sur trois adresses.
fn choose_address(addrs: &[std::net::IpAddr], local_prefix: Option<&str>) -> String {
    let mut v4: Vec<std::net::Ipv4Addr> = addrs
        .iter()
        .filter_map(|ip| match ip {
            std::net::IpAddr::V4(v4) => Some(*v4),
            std::net::IpAddr::V6(_) => None,
        })
        .collect();
    v4.sort_unstable();

    let is_private = |v4: &std::net::Ipv4Addr| {
        let o = v4.octets();
        o[0] == 192 || o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1]))
    };

    let same_subnet = local_prefix.and_then(|prefix| {
        v4.iter()
            .find(|v| v.to_string().starts_with(prefix))
            .map(|v| v.to_string())
    });

    same_subnet
        .or_else(|| v4.iter().find(|v| is_private(v)).map(|v| v.to_string()))
        .or_else(|| v4.first().map(|v| v.to_string()))
        .unwrap_or_else(|| {
            // Que de l'IPv6 : on trie là aussi plutôt que de prendre au hasard.
            let mut rest: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
            rest.sort();
            rest.into_iter().next().unwrap_or_default()
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

// ---------------------------------------------------------------------------
// AirPlay `features` / `flags` bitmask parsing.
// ---------------------------------------------------------------------------

/// `flags` TXT bit meaning "the receiver requires a PIN / password".
/// (RAOP/AirPlay `flags`, bit 9.)
const AIRPLAY_FLAG_PIN_REQUIRED: u64 = 0x200;

// AirPlay feature bits relevant to whether pairing is mandatory. The `features`
// value is a 64-bit mask; the low 32 bits are the first word and the high 32
// bits the second (see the `features` TXT record used by RAOP/AirPlay 2).
//
//   bit 26 — SupportsSystemPairing / PairSetupAndMFi
//   bit 27 — SupportsUnifiedPairSetupAndMFi
//   bit 46 — SupportsCoreUtilsPairingAndEncryption
//   bit 51 — SupportsUnifiedPairVerify (AirPlay 2 access control)
const FT_BIT_SYSTEM_PAIRING: u64 = 1 << 26;
const FT_BIT_UNIFIED_PAIR_SETUP: u64 = 1 << 27;
const FT_BIT_COREUTILS_PAIR_ENC: u64 = 1 << 46;
const FT_BIT_UNIFIED_PAIR_VERIFY: u64 = 1 << 51;

/// Parse a single hex or decimal integer TXT value like `0x200` or `514`.
fn parse_hex_u64(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Parse the AirPlay `features` TXT record into a single 64-bit mask.
///
/// The record appears in two shapes in the wild:
///   * one word:  `0x5A7FFFF7`
///   * two words: `0x5A7FFFF7,0x1E`  (low word first, high word second)
/// Returns `None` if nothing parses.
pub fn parse_airplay_features(raw: &str) -> Option<u64> {
    let mut parts = raw.split(',');
    let low = parse_hex_u64(parts.next()?.trim())?;
    match parts.next() {
        Some(high_str) => {
            let high = parse_hex_u64(high_str.trim())?;
            Some((high << 32) | (low & 0xFFFF_FFFF))
        }
        None => Some(low),
    }
}

/// Whether the parsed `features` mask indicates the receiver mandates a
/// HomeKit-style pair-setup/pair-verify before accepting an RTSP session.
pub fn airplay_requires_pairing(features: u64) -> bool {
    features
        & (FT_BIT_SYSTEM_PAIRING
            | FT_BIT_UNIFIED_PAIR_SETUP
            | FT_BIT_COREUTILS_PAIR_ENC
            | FT_BIT_UNIFIED_PAIR_VERIFY)
        != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_features_single_and_double_word() {
        // Single 32-bit word.
        assert_eq!(parse_airplay_features("0x5A7FFFF7"), Some(0x5A7F_FFF7));
        // Two words: high word shifted into bits 32..63.
        assert_eq!(
            parse_airplay_features("0x5A7FFFF7,0x1E"),
            Some((0x1E << 32) | 0x5A7F_FFF7)
        );
        // Decimal fallback.
        assert_eq!(parse_airplay_features("514"), Some(514));
        // Garbage → None.
        assert_eq!(parse_airplay_features("nope"), None);
    }

    #[test]
    fn features_pairing_bit_detection() {
        // Bit 27 set → requires pairing (typical AirPlay 2 receiver / TV).
        let with_pairing = FT_BIT_UNIFIED_PAIR_SETUP | 0xFF;
        assert!(airplay_requires_pairing(with_pairing));
        // No pairing bits → legacy AirPlay 1, no pairing.
        assert!(!airplay_requires_pairing(0x0000_00FF));
        // Bit 51 (high word) also triggers.
        assert!(airplay_requires_pairing(FT_BIT_UNIFIED_PAIR_VERIFY));

        // End-to-end via the string parser: a two-word features value whose
        // high word carries bit 51 (bit 19 of the high word).
        let raw = format!("0x000000FF,0x{:X}", 1u64 << 19);
        let bits = parse_airplay_features(&raw).unwrap();
        assert!(airplay_requires_pairing(bits));
    }

    #[test]
    fn pin_flag_constant() {
        assert_eq!(AIRPLAY_FLAG_PIN_REQUIRED, 0x200);
    }

    #[test]
    fn service_constants_end_with_local() {
        assert!(AIRPLAY_SERVICE.ends_with(".local."));
        assert!(BLUOS_SERVICE.ends_with(".local."));
        assert!(BLUOS_SERVICE2.ends_with(".local."));
        assert!(BLUOS_SERVICE3.ends_with(".local."));
        assert!(CHROMECAST_SERVICE.ends_with(".local."));
        assert!(SQUEEZEBOX_SERVICE.ends_with(".local."));
        assert!(TUNE_SERVICE.ends_with(".local."));
    }

    fn appareil(id: &str, host: &str, mac: Option<&str>, t: OutputType) -> DiscoveredDevice {
        let mut d = DiscoveredDevice::new(id.into(), "SALON".into(), t, host.into(), 7000);
        d.mac_address = mac.map(str::to_string);
        d
    }

    #[test]
    fn le_nom_raop_porte_la_mac_normalisee() {
        assert_eq!(
            mac_depuis_nom_raop("800A805D4DEE@DMP-A8").as_deref(),
            Some("80:0A:80:5D:4D:EE")
        );
        // Pas de préfixe MAC : rien à inventer.
        assert_eq!(mac_depuis_nom_raop("Mac Studio"), None);
        assert_eq!(mac_depuis_nom_raop("pasunemac@Salon"), None);
    }

    #[test]
    fn une_enceinte_vue_en_raop_et_airplay_reste_un_seul_appareil() {
        // Le cœur de #197 : `_raop` résolu sur une adresse, `_airplay` sur une
        // autre — même MAC, donc même enceinte. Avant, la comparaison ne
        // portait que sur l'adresse et fabriquait deux sorties « SALON ».
        let stocke = appareil(
            "airplay-80:0A:80:5D:4D:EE",
            "192.168.1.50",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        let nouveau = appareil(
            "airplay-2a02:842a::17bc-7000",
            "2a02:842a:3cca:d601:1d57:b33:38e8:17bc",
            Some("800A805D4DEE"), // graphie du nom RAOP, sans séparateurs
            OutputType::Airplay,
        );
        // L'adresse stockée (IPv4) vaut mieux que l'annonce (IPv6) : on jette.
        assert_eq!(
            verdict_doublon_airplay([&stocke].into_iter(), &nouveau),
            DoublonAirplay::Ignorer
        );
    }

    #[test]
    fn une_ipv4_tardive_repare_l_appareil_ancre_sur_une_ipv6() {
        // L'erreur du terrain (#197) : « airplay connect 2a02:…:7000: Network
        // is unreachable (os error 101) ». L'appareil retenu est ancré sur une
        // IPv6 que Docker ne route pas ; quand l'autre service apporte enfin
        // l'IPv4, il faut REPRENDRE l'appareil en place — même identifiant,
        // donc même zone — et non jeter l'annonce.
        let stocke = appareil(
            "airplay-2a02:842a:3cca:d601:1d57:b33:38e8:17bc-7000",
            "2a02:842a:3cca:d601:1d57:b33:38e8:17bc",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        let nouveau = appareil(
            "airplay-80:0A:80:5D:4D:EE",
            "192.168.1.50",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        assert_eq!(
            verdict_doublon_airplay([&stocke].into_iter(), &nouveau),
            DoublonAirplay::Reprendre {
                id_retenu: stocke.id.clone()
            }
        );
    }

    #[test]
    fn les_deux_enceintes_d_une_paire_restent_deux_appareils() {
        // Deux MAC différentes, deux adresses différentes : rien ne permet de
        // les confondre, même si elles portent le même nom (« SALON »).
        let gauche = appareil(
            "airplay-80:0A:80:5D:4D:EE",
            "192.168.1.50",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        let droite = appareil(
            "airplay-80:0A:80:5D:4D:F0",
            "192.168.1.51",
            Some("80:0A:80:5D:4D:F0"),
            OutputType::Airplay,
        );
        assert_eq!(
            verdict_doublon_airplay([&gauche].into_iter(), &droite),
            DoublonAirplay::Nouveau
        );
    }

    #[test]
    fn le_dedoublonnage_par_adresse_est_conserve() {
        // Comportement historique : même hôte, identifiants différents
        // (`_raop` sans TXT deviceid contre `_airplay` avec) → doublon.
        let stocke = appareil(
            "airplay-192.168.1.50-7000",
            "192.168.1.50",
            None,
            OutputType::Airplay,
        );
        let nouveau = appareil(
            "airplay-80:0A:80:5D:4D:EE",
            "192.168.1.50",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        assert_eq!(
            verdict_doublon_airplay([&stocke].into_iter(), &nouveau),
            DoublonAirplay::Ignorer
        );
    }

    #[test]
    fn un_appareil_d_un_autre_protocole_ne_compte_pas_comme_doublon() {
        // La zone DLNA « Salon » du même hôte n'est pas un doublon AirPlay.
        let dlna = appareil(
            "dlna-192.168.1.50-1400",
            "192.168.1.50",
            None,
            OutputType::Dlna,
        );
        let nouveau = appareil(
            "airplay-80:0A:80:5D:4D:EE",
            "192.168.1.50",
            Some("80:0A:80:5D:4D:EE"),
            OutputType::Airplay,
        );
        assert_eq!(
            verdict_doublon_airplay([&dlna].into_iter(), &nouveau),
            DoublonAirplay::Nouveau
        );
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

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn choose_address_is_the_same_whatever_the_order() {
        // Le coeur de #1528 : deux adresses egalement recevables sur le meme
        // sous-reseau. L'appelant itere un HashSet, donc l'ordre varie d'un
        // demarrage a l'autre — l'identite de l'appareil, elle, ne doit pas.
        let a = [ip("192.168.1.42"), ip("192.168.1.77")];
        let b = [ip("192.168.1.77"), ip("192.168.1.42")];
        assert_eq!(
            choose_address(&a, Some("192.168.1.")),
            choose_address(&b, Some("192.168.1.")),
        );
    }

    #[test]
    fn choose_address_prefers_the_local_subnet_then_private_then_the_rest() {
        let addrs = [ip("8.8.8.8"), ip("10.0.0.5"), ip("192.168.1.42")];
        assert_eq!(choose_address(&addrs, Some("192.168.1.")), "192.168.1.42");
        // Hors sous-reseau connu, une privee vaut mieux qu'une publique.
        assert_eq!(choose_address(&addrs, Some("172.20.")), "10.0.0.5");
        // Aucune privee : il reste la publique, plutot que rien.
        assert_eq!(choose_address(&[ip("8.8.8.8")], None), "8.8.8.8");
    }

    #[test]
    fn choose_address_falls_back_to_ipv6_without_drawing_lots() {
        let a = [ip("fe80::2"), ip("fe80::1")];
        let b = [ip("fe80::1"), ip("fe80::2")];
        assert_eq!(choose_address(&a, None), choose_address(&b, None));
        assert!(!choose_address(&a, None).is_empty());
    }

    #[test]
    fn choose_address_without_any_address_is_empty_not_a_panic() {
        assert_eq!(choose_address(&[], Some("192.168.1.")), "");
    }

    #[test]
    fn device_id_prefers_what_the_device_announces() {
        // Le coeur de #1528 : deux adresses differentes, meme appareil, meme
        // identifiant. C'est ce qui empeche un bail DHCP renouvele de dedoubler
        // la zone et de faire revenir celles qu'on a supprimees.
        let a = device_id_for(
            OutputType::Chromecast,
            Some("uuid-abc"),
            "192.168.1.42",
            8009,
        );
        let b = device_id_for(
            OutputType::Chromecast,
            Some("uuid-abc"),
            "192.168.1.77",
            8009,
        );
        assert_eq!(a, b);
    }

    #[test]
    fn device_id_falls_back_to_the_address_when_nothing_is_announced() {
        // Tous les appareils n'annoncent pas d'identifiant : on ne peut pas
        // faire mieux que l'adresse, et c'est la forme historique.
        assert_eq!(
            device_id_for(OutputType::Dlna, None, "192.168.1.9", 8080),
            legacy_device_id(OutputType::Dlna, "192.168.1.9", 8080),
        );
    }

    #[test]
    fn two_devices_announcing_different_ids_never_collide() {
        // L'autre bord : meme adresse et meme port (un hote qui expose deux
        // services), identifiants distincts.
        assert_ne!(
            device_id_for(OutputType::Airplay, Some("aa:bb"), "192.168.1.5", 7000),
            device_id_for(OutputType::Airplay, Some("cc:dd"), "192.168.1.5", 7000),
        );
    }

    #[test]
    fn the_legacy_form_is_unchanged() {
        // Les zones creees avant #1528 sont enregistrees sous cette forme
        // exacte : la decouverte s'en sert pour les retrouver et les
        // re-ancrer. La changer perdrait toutes les zones existantes.
        assert_eq!(
            legacy_device_id(OutputType::Bluos, "192.168.1.23", 11000),
            format!("{}-192.168.1.23-11000", OutputType::Bluos),
        );
    }
}
