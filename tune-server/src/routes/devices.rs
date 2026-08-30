use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::dedup_devices;
use tune_core::discovery::renderer_identity::{
    Evidence, IdentityVerdict, RendererIdentity, compare_at_same_location,
};
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::outputs::bluos::BluosOutput;
use tune_core::outputs::dlna::DlnaOutput;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/list", get(list_devices))
        // Catalogue statique marque→modèles (+ quirks) pour l'UI de config zone.
        .route("/catalog", get(device_catalog))
        .route("/add", post(add_device))
        .route("/scan", post(scan_devices))
        .route("/rescan", post(rescan_local_devices))
        .route("/audio", get(list_audio_devices))
        .route("/audio/asio-devices", get(list_asio_devices))
        // buffer-stats/all must be registered before /{device_id} to avoid capture
        .route("/buffer-stats/all", get(all_buffer_stats))
        .route("/{device_id}/status", get(device_status))
        .route("/{device_id}/buffer-stats", get(device_buffer_stats))
        .route(
            "/{device_id}/buffer",
            axum::routing::patch(set_device_buffer),
        )
        .route("/clear", post(clear_devices))
        // #1280 — « ignorer cet appareil ». Statique avant dynamique : axum
        // donne déjà la priorité au segment littéral, la ligne est ici pour
        // que la lecture le montre.
        .route("/ignored", get(list_ignored_devices))
        .route(
            "/{device_id}/ignore",
            post(ignore_device).delete(unignore_device),
        )
        .route("/{device_id}", axum::routing::delete(delete_device))
        .route("/{device_id}/pair", post(pair_device))
        .route("/{device_id}/pair/pin", post(pair_device_pin))
        .route(
            "/{device_id}/airplay2/pair-pin-start",
            post(airplay2_pair_pin_start),
        )
}

/// Catalogue statique des appareils (marque → modèles + profils de quirks).
/// Donnée versionnée embarquée dans le binaire ; sert à peupler les menus
/// déroulants Marque/Modèle de la config d'une zone.
async fn device_catalog() -> Json<Value> {
    Json(json!(tune_core::device_catalog::catalog()))
}

async fn list_devices(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let discovered = scanner.devices().await;

    let outputs = state.outputs.lock().await;
    let registered_ids: std::collections::HashSet<String> = outputs.list().into_iter().collect();
    // Use info_all() instead of status_all() to avoid sequential is_available() probes
    // that can block for seconds per unreachable DLNA device, causing the entire
    // endpoint to time out and return 0 DLNA devices.
    let all_output_info = outputs.info_all().await;
    drop(outputs);

    let mut items = build_device_list(discovered, &registered_ids, &all_output_info);
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    mark_hidden_zones(&mut items, &zone_repo);
    // #1280 — les appareils ignorés sortent de la liste. Le scanner continue
    // de les ENTENDRE (Patatorz : « ils peuvent rester détectables ») ; ils
    // cessent d'être PROPOSÉS.
    retirer_appareils_ignores(&mut items, &state.backend);

    Json(json!(items))
}

/// L'identité d'une entrée de `GET /devices`, telle qu'elle est sérialisée.
fn identite_de_l_entree(item: &Value) -> tune_core::db::ignored_device_repo::DeviceIdentity<'_> {
    let s = |cle: &str| item.get(cle).and_then(Value::as_str).unwrap_or("");
    tune_core::db::ignored_device_repo::DeviceIdentity::new(s("id"), s("host"), s("name"))
        .with_mac(item.get("mac_address").and_then(Value::as_str))
}

/// Retire de la liste les appareils ignorés (#1280).
///
/// Les identités SECONDAIRES comptent : `build_device_list` replie les
/// jumelles SSDP d'un même appareil dans `capabilities.alternatives`, et
/// c'est peut-être l'une d'elles que l'utilisateur a fait taire — sans cette
/// boucle, le Sonos resterait listé sous son identité primaire.
fn retirer_appareils_ignores(items: &mut Vec<Value>, db: &Arc<dyn DbBackend>) {
    let repo = tune_core::db::ignored_device_repo::IgnoredDeviceRepo::with_backend(db.clone());
    let ignores = match repo.list() {
        Ok(ignores) if !ignores.is_empty() => ignores,
        // Liste vide, ou base pré-migration : rien à retirer.
        _ => return,
    };
    items.retain(|item| {
        let principale = identite_de_l_entree(item);
        if ignores
            .iter()
            .any(|e| tune_core::db::ignored_device_repo::identity_matches(e, principale))
        {
            return false;
        }
        let alternatives = item
            .get("capabilities")
            .and_then(|c| c.get("alternatives"))
            .and_then(Value::as_array);
        let Some(alternatives) = alternatives else {
            return true;
        };
        !alternatives.iter().any(|alt| {
            let identite = tune_core::db::ignored_device_repo::DeviceIdentity::new(
                alt.get("id").and_then(Value::as_str).unwrap_or(""),
                principale.host,
                alt.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(principale.name),
            )
            .with_mac(principale.mac);
            ignores
                .iter()
                .any(|e| tune_core::db::ignored_device_repo::identity_matches(e, identite))
        })
    });
}

/// Rend visible l'état que la découverte connaissait seulement dans ses logs.
///
/// Un appareil dédoublonné peut porter plusieurs identités. On conserve donc
/// l'identité exacte de la zone masquée : le client doit la transmettre à la
/// route de création pour que celle-ci démasque la bonne ligne et récupère ses
/// réglages, au lieu de créer une nouvelle zone sur l'identité primaire.
fn mark_hidden_zones(items: &mut [Value], zone_repo: &ZoneRepo) {
    for item in items {
        let hidden_device_id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| zone_repo.is_device_hidden(id))
            .map(str::to_owned)
            .or_else(|| {
                item.get("capabilities")
                    .and_then(|c| c.get("alternatives"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|alternative| alternative.get("id").and_then(Value::as_str))
                    .find(|id| zone_repo.is_device_hidden(id))
                    .map(str::to_owned)
            });

        if let Some(obj) = item.as_object_mut() {
            obj.insert("zone_hidden".into(), json!(hidden_device_id.is_some()));
            if let Some(device_id) = hidden_device_id {
                obj.insert("hidden_zone_device_id".into(), json!(device_id));
            }
        }
    }
}

/// Construit la liste renvoyée par `GET /devices` (et `/devices/list`).
///
/// Logique extraite du handler pour être testable sans `AppState`.
fn build_device_list(
    discovered: Vec<tune_core::discovery::device::DiscoveredDevice>,
    registered_ids: &std::collections::HashSet<String>,
    all_output_info: &[Value],
) -> Vec<Value> {
    // Même dédoublonnage que POST /devices/scan : un appareil qui s'annonce
    // sous plusieurs identités (mDNS + SSDP, cf. #1880) est regroupé par hôte,
    // les identités secondaires rabattues dans capabilities["alternatives"].
    // Sans ce repli, GET /devices renvoyait chaque identité comme une entrée
    // distincte et la barre latérale affichait l'appareil en double (#2452).
    let deduped = dedup_devices(discovered);

    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut items: Vec<Value> = deduped
        .iter()
        .map(|d| {
            seen_ids.insert(d.id.clone());
            let mut registered = registered_ids.contains(&d.id);
            let mut output_info = all_output_info
                .iter()
                .find(|info| info.get("device_id").and_then(Value::as_str) == Some(d.id.as_str()));
            // Les identités secondaires comptent comme « vues » : la boucle de
            // rattrapage ci-dessous ne doit pas les réintroduire. Et si l'une
            // d'elles est enregistrée comme sortie, l'appareil l'est.
            if let Some(alts) = d
                .capabilities
                .get("alternatives")
                .and_then(|a| a.as_array())
            {
                for alt_id in alts.iter().filter_map(|a| a.get("id")?.as_str()) {
                    seen_ids.insert(alt_id.to_string());
                    registered = registered || registered_ids.contains(alt_id);
                    if output_info.is_none() {
                        output_info = all_output_info.iter().find(|info| {
                            info.get("device_id").and_then(Value::as_str) == Some(alt_id)
                        });
                    }
                }
            }
            let mut v = serde_json::to_value(d).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), json!(true));
                obj.insert("registered".into(), json!(registered));
                obj.insert("type".into(), json!(d.device_type.to_string()));
                if let Some(capabilities) = output_info
                    .and_then(|info| info.get("output_capabilities"))
                    .cloned()
                {
                    obj.insert("output_capabilities".into(), capabilities);
                }
            }
            v
        })
        .collect();

    // Add any registered outputs not already present from SSDP discovery.
    // This ensures DLNA/OpenHome devices appear even when the SSDP scanner's
    // internal device list is empty (e.g., between scan cycles or after restart).
    for output_info in all_output_info {
        if let Some(device_id) = output_info.get("device_id").and_then(|v| v.as_str()) {
            if seen_ids.contains(device_id) {
                continue;
            }
            seen_ids.insert(device_id.to_string());
            let name = output_info
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let output_type = output_info
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let host = output_info
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            items.push(json!({
                "id": device_id,
                "name": name,
                "type": output_type,
                "host": host,
                "port": 0,
                "available": true,
                "registered": true,
                "output_capabilities": output_info.get("output_capabilities").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    items
}

// ---------------------------------------------------------------------------
// Manual Device Addition
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AddDeviceRequest {
    r#type: String,
    host: String,
    port: Option<u16>,
    name: Option<String>,
}

/// Settings key holding the JSON array of manually-added devices.
const MANUAL_DEVICES_KEY: &str = "manual_devices";

/// A device the user added by hand via `POST /devices/add`.
///
/// These are persisted (see [`persist_manual_device`]) and re-registered on
/// startup by [`reregister_manual_devices`].  Persistence matters because
/// legacy renderers that don't answer SSDP M-SEARCH (e.g. the Cyrus Stream X)
/// never resurface through normal discovery, so without this they vanish on
/// every restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualDevice {
    pub r#type: String,
    pub host: String,
    pub port: u16,
    pub name: Option<String>,
}

impl ManualDevice {
    fn device_id(&self) -> String {
        format!("{}-{}-{}", self.r#type.to_lowercase(), self.host, self.port)
    }
}

fn load_manual_devices(state: &AppState) -> Vec<ManualDevice> {
    let repo = SettingsRepo::with_backend(state.backend.clone());
    match repo.get(MANUAL_DEVICES_KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn save_manual_devices(state: &AppState, devices: &[ManualDevice]) {
    let repo = SettingsRepo::with_backend(state.backend.clone());
    match serde_json::to_string(devices) {
        Ok(json) => {
            if let Err(e) = repo.set(MANUAL_DEVICES_KEY, &json) {
                warn!(error = %e, "manual_devices_persist_failed");
            }
        }
        Err(e) => warn!(error = %e, "manual_devices_serialize_failed"),
    }
}

/// Persist a manual device, replacing any existing entry with the same id.
fn persist_manual_device(state: &AppState, dev: &ManualDevice) {
    let id = dev.device_id();
    let mut devices = load_manual_devices(state);
    devices.retain(|d| d.device_id() != id);
    devices.push(dev.clone());
    save_manual_devices(state, &devices);
}

/// Drop a manual device from persistence by its device id (no-op if absent).
fn forget_manual_device(state: &AppState, device_id: &str) {
    let mut devices = load_manual_devices(state);
    let before = devices.len();
    devices.retain(|d| d.device_id() != device_id);
    if devices.len() != before {
        save_manual_devices(state, &devices);
    }
}

fn ensure_zone(state: &AppState, name: &str, type_str: &str, device_id: &str) -> Option<i64> {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    match zone_repo.get_or_create(name, Some(type_str), device_id) {
        Ok((zid, created)) => {
            if !created {
                let _ = zone_repo.set_online_by_device(device_id, true);
            }
            Some(zid)
        }
        Err(_) => None,
    }
}

/// Probe a manually-specified device, register its output, and ensure a zone
/// exists.  Shared by the `POST /devices/add` route and the startup
/// re-registration path.  Returns `(device_id, resolved_name, zone_id)`.
pub async fn register_manual_device(
    state: &AppState,
    dev: &ManualDevice,
) -> Result<(String, String, Option<i64>), String> {
    let device_id = dev.device_id();
    match dev.r#type.to_lowercase().as_str() {
        "bluos" => {
            let probe_url = format!("http://{}:{}/Status", dev.host, dev.port);
            let resp = state
                .http_client
                .get(&probe_url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .map_err(|e| {
                    format!(
                        "Cannot reach BluOS device at {}:{}: {e}",
                        dev.host, dev.port
                    )
                })?;
            if !resp.status().is_success() {
                return Err(format!(
                    "BluOS device at {}:{} responded with status {}",
                    dev.host,
                    dev.port,
                    resp.status()
                ));
            }
            let xml = resp.text().await.unwrap_or_default();
            let device_name = dev.name.clone().unwrap_or_else(|| {
                extract_xml_tag(&xml, "name")
                    .or_else(|| extract_xml_tag(&xml, "modelName"))
                    .unwrap_or_else(|| format!("BluOS {}", dev.host))
            });

            let bluos = BluosOutput::new(
                device_name.clone(),
                device_id.clone(),
                dev.host.clone(),
                dev.port,
            );
            state.outputs.lock().await.register(Box::new(bluos));

            let zone_id = ensure_zone(state, &device_name, "bluos", &device_id);
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::DeviceDiscovered,
                json!({ "device_id": device_id, "name": device_name, "device_type": "bluos", "host": dev.host }),
            );
            info!(name = %device_name, id = %device_id, host = %dev.host, port = dev.port, "manual_bluos_device_registered");
            Ok((device_id, device_name, zone_id))
        }
        "dlna" => {
            let location = format!("http://{}:{}/description.xml", dev.host, dev.port);
            let desc = fetch_device_description(&location).await.map_err(|e| {
                format!(
                    "Cannot fetch DLNA description from {}:{}: {e}",
                    dev.host, dev.port
                )
            })?;
            if !desc.is_media_renderer() {
                return Err(format!(
                    "Device at {}:{} is not a DLNA Media Renderer",
                    dev.host, dev.port
                ));
            }
            let service_urls = desc.service_urls();
            let (Some(av), Some(rc)) = (
                service_urls.get("avtransport"),
                service_urls.get("renderingcontrol"),
            ) else {
                return Err(
                    "Device is a media renderer but missing AVTransport or RenderingControl services"
                        .to_string(),
                );
            };

            let base = format!("http://{}:{}", dev.host, dev.port);
            let device_name = dev
                .name
                .clone()
                .unwrap_or_else(|| format!("DLNA {}", dev.host));
            let delay = crate::config::resolve_play_delay(
                &state.backend,
                &state.config,
                &device_id,
                &device_name,
            );
            let cm_url = service_urls
                .get("connectionmanager")
                .or_else(|| service_urls.get("ConnectionManager"))
                .map(|p| format!("{base}{p}"));

            let dlna = DlnaOutput::new(
                device_name.clone(),
                device_id.clone(),
                dev.host.clone(),
                format!("{base}{av}"),
                format!("{base}{rc}"),
                cm_url,
            )
            .with_play_delay(delay);
            state.outputs.lock().await.register(Box::new(dlna));

            let zone_id = ensure_zone(state, &device_name, "dlna", &device_id);
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::DeviceDiscovered,
                json!({ "device_id": device_id, "name": device_name, "device_type": "dlna", "host": dev.host }),
            );
            info!(name = %device_name, id = %device_id, host = %dev.host, port = dev.port, "manual_dlna_device_registered");
            Ok((device_id, device_name, zone_id))
        }
        other => Err(format!(
            "Unsupported device type: '{other}'. Supported: bluos, dlna"
        )),
    }
}

/// Re-registration runs very early in boot — before the HTTP server even
/// binds — so a device (or the local network stack) that isn't reachable in
/// that exact window would otherwise be lost until the next restart. Retry
/// each device with exponential backoff to ride out that race.
const REREGISTER_MAX_ATTEMPTS: u32 = 8;
const REREGISTER_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const REREGISTER_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Re-register every persisted manual device at startup. Each device is
/// retried independently (in its own task) with exponential backoff, so an
/// unreachable device neither blocks the others nor delays boot.
pub async fn reregister_manual_devices(state: &AppState) {
    let devices = load_manual_devices(state);
    if devices.is_empty() {
        return;
    }
    info!(count = devices.len(), "reregistering_manual_devices");
    for dev in devices {
        let state = state.clone();
        tokio::spawn(async move { reregister_with_backoff(&state, dev).await });
    }
}

/// Try to register one manual device, retrying with exponential backoff
/// (1s, 2s, 4s … capped at 60s) until it succeeds or attempts are exhausted.
async fn reregister_with_backoff(state: &AppState, dev: ManualDevice) {
    let mut delay = REREGISTER_BASE_DELAY;
    for attempt in 1..=REREGISTER_MAX_ATTEMPTS {
        match register_manual_device(state, &dev).await {
            Ok((id, name, _)) => {
                info!(id = %id, name = %name, attempt, "manual_device_reregistered");
                return;
            }
            Err(e) if attempt == REREGISTER_MAX_ATTEMPTS => {
                warn!(
                    host = %dev.host,
                    port = dev.port,
                    r#type = %dev.r#type,
                    attempts = attempt,
                    error = %e,
                    "manual_device_reregister_gave_up"
                );
                return;
            }
            Err(e) => {
                warn!(
                    host = %dev.host,
                    port = dev.port,
                    r#type = %dev.r#type,
                    attempt,
                    retry_in_s = delay.as_secs(),
                    error = %e,
                    "manual_device_reregister_retry"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(REREGISTER_MAX_DELAY);
            }
        }
    }
}

// ── Auto-discovered DLNA renderers: persist + reprobe (#1126) ──────────────
//
// Some renderers (Cyrus Stream X2) don't answer SSDP M-SEARCH while idle and
// only rarely emit `ssdp:alive`, so after a restart they never resurface via
// multicast and their zone stays offline indefinitely — every `play` rejected.
// They ARE reachable though (ping, description.xml over HTTP). So we persist
// each auto-discovered DLNA renderer's LOCATION + UUID and, at startup, probe
// the LOCATION directly over HTTP (verifying the UUID still matches) to
// re-register it alongside multicast. The registry is keyed by the `uuid:…`
// device_id, so whichever path wins re-attaches the SAME zone (no duplicate).

/// Settings key holding the JSON array of auto-discovered DLNA renderers.
const DISCOVERED_DLNA_KEY: &str = "discovered_dlna_devices";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDlnaDevice {
    /// `uuid:…` — the DLNA device_id; keying the registry by it re-attaches the
    /// existing zone rather than creating a duplicate.
    uuid: String,
    /// Full description.xml URL as advertised by SSDP (may use a non-standard
    /// path/port); used verbatim for the HTTP reprobe.
    location: String,
    name: String,
    host: String,
    port: u16,
    // ── Clés de reconnaissance, ajoutées par #2639 ────────────────────────
    //
    // Un UDN peut être régénéré par un redémarrage ou une mise à jour de
    // micrologiciel. Ces trois clés-là ne le sont pas, et permettent de
    // reconnaître l'appareil malgré la bascule sans confondre deux exemplaires
    // (cf. `tune_core::discovery::renderer_identity`).
    //
    // `#[serde(default)]` : les magasins écrits avant #2639 ne les portent pas
    // et doivent continuer de se relire — sans quoi `serde_json::from_str`
    // échouerait et `load_discovered_dlna` rendrait un magasin VIDE, ce qui
    // effacerait toutes les zones d'un coup. Elles se remplissent au premier
    // re-sondage réussi.
    #[serde(default)]
    mac: String,
    #[serde(default)]
    manufacturer: String,
    #[serde(default)]
    model: String,
}

impl DiscoveredDlnaDevice {
    /// Entrée sans clé de reconnaissance — l'état d'un magasin d'avant #2639.
    pub fn new(uuid: &str, location: &str, name: &str, host: &str, port: u16) -> Self {
        Self {
            uuid: uuid.to_string(),
            location: location.to_string(),
            name: name.to_string(),
            host: host.to_string(),
            port,
            mac: String::new(),
            manufacturer: String::new(),
            model: String::new(),
        }
    }

    /// Ajoute ce que la description (et l'ARP encore chaud) viennent d'apprendre.
    pub fn with_identity(mut self, mac: &str, manufacturer: &str, model: &str) -> Self {
        self.mac = mac.to_string();
        self.manufacturer = manufacturer.to_string();
        self.model = model.to_string();
        self
    }

    fn identity(&self) -> RendererIdentity<'_> {
        RendererIdentity {
            udn: &self.uuid,
            mac: &self.mac,
            friendly_name: &self.name,
            manufacturer: &self.manufacturer,
            model_name: &self.model,
        }
    }
}

fn load_discovered_dlna(backend: &Arc<dyn DbBackend>) -> Vec<DiscoveredDlnaDevice> {
    let repo = SettingsRepo::with_backend(backend.clone());
    match repo.get(DISCOVERED_DLNA_KEY) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn save_discovered_dlna(backend: &Arc<dyn DbBackend>, devices: &[DiscoveredDlnaDevice]) {
    let repo = SettingsRepo::with_backend(backend.clone());
    match serde_json::to_string(devices) {
        Ok(json) => {
            if let Err(e) = repo.set(DISCOVERED_DLNA_KEY, &json) {
                warn!(error = %e, "discovered_dlna_persist_failed");
            }
        }
        Err(e) => warn!(error = %e, "discovered_dlna_serialize_failed"),
    }
}

/// Persist (upsert by uuid) an auto-discovered DLNA renderer so it can be
/// re-probed after a restart. Called from the SSDP discovery path once a
/// renderer with a LOCATION is registered. Skips the settings write when
/// nothing changed, so a chatty `ssdp:alive` stream costs nothing.
pub fn persist_discovered_dlna(backend: &Arc<dyn DbBackend>, entry: &DiscoveredDlnaDevice) {
    let (uuid, location, name, host) = (
        entry.uuid.as_str(),
        entry.location.as_str(),
        entry.name.as_str(),
        entry.host.as_str(),
    );
    let mut devices = load_discovered_dlna(backend);
    if devices.iter().any(|d| {
        d.uuid == uuid
            && d.location == location
            && d.name == name
            && d.host == host
            // Les clés de reconnaissance (#2639) font partie de l'entrée : une
            // MAC ou un modèle fraîchement relevés doivent être écrits, sinon
            // le magasin resterait éternellement au niveau « nom seul ».
            && d.mac == entry.mac
            && d.manufacturer == entry.manufacturer
            && d.model == entry.model
    }) {
        return;
    }
    // Une `LOCATION` = une description racine = UN appareil physique (UPnP).
    // Un HEOS Denon/Marantz annonce sa racine et chacun de ses appareils
    // embarques sous des `uuid:` differents mais a la MEME `LOCATION` : sans
    // ce `d.location != location`, le magasin gagnait une entree par UDN —
    // cinq lecteurs pour un seul ND8006, re-sondes huit fois chacun au
    // demarrage (#1703).
    devices.retain(|d| d.uuid != uuid && d.location != location);
    devices.push(entry.clone());
    save_discovered_dlna(backend, &devices);
}

/// Pourquoi un re-sondage n'a pas abouti — et ce qu'il faut en faire.
///
/// Avant #2639, `register_discovered_dlna` rendait un `Err(String)` que
/// l'appelant reniflait par sous-chaine (`DEFINITIVE_MARKERS`). Deux causes
/// sans aucun rapport — « ce n'est pas un lecteur » et « l'identifiant a
/// change » — partageaient donc la meme branche, le meme traitement
/// (effacement) et le MEME message de journal
/// (`discovered_dlna_forgotten_not_a_renderer`). Un Marantz ND8006 bien
/// vivant disparaissait ainsi sous un message qui annoncait l'autre panne.
///
/// Le type porte maintenant la decision, pas le texte : ajouter une cause
/// oblige a dire ce qu'on en fait, et un message ne peut plus en couvrir deux.
#[derive(Debug)]
enum ReprobeFailure {
    /// L'appareil n'a pas repondu, ou mal : la panne peut etre passagere, on
    /// retente avec le backoff.
    Unreachable(String),
    /// L'appareil a repondu et n'expose plus de quoi jouer. Aucun reessai n'y
    /// changera rien : l'entree part.
    NotARenderer,
    /// L'identifiant a change ET une cle stable prouve que c'est un AUTRE
    /// materiel a cette adresse. L'entree part, en nommant l'intrus.
    Replaced {
        observed_udn: String,
        observed_name: String,
        evidence: Evidence,
    },
    /// L'identifiant a change et rien ne permet de trancher. On ne detruit
    /// RIEN : l'entree reste, elle sera resondee.
    IdentityUnclear { observed_udn: String },
    /// #1280 — l'utilisateur a fait taire cet appareil. Rien n'est sonde, et
    /// surtout l'entree RESTE : le deblocage doit pouvoir la ressusciter au
    /// demarrage suivant, sans quoi ignorer serait une suppression deguisee.
    Ignore,
}

impl ReprobeFailure {
    /// L'entree persistee doit-elle disparaitre du magasin ?
    ///
    /// Tout #2639 tient dans cette table. Une entree qui part, c'est une zone
    /// que l'utilisateur perd : on ne la retire que sur une preuve — l'appareil
    /// a repondu et n'est plus un lecteur, ou une cle stable designe un AUTRE
    /// materiel. Un desaccord d'UDN qu'on ne sait pas expliquer ne retire
    /// RIEN : une zone configuree ne se perd pas sur une ignorance.
    fn retire_l_entree(&self) -> bool {
        match self {
            Self::NotARenderer | Self::Replaced { .. } => true,
            Self::Unreachable(_) | Self::IdentityUnclear { .. } | Self::Ignore => false,
        }
    }
}

/// Journalise une reponse que l'appareil a donnee et qu'on a comprise, puis
/// applique la seule decision qui compte pour l'utilisateur : son entree part,
/// ou elle reste.
///
/// Chaque cause a SON message. C'est le fond de #2639 : « pas un lecteur » et
/// « identite changee » ne menent pas au meme endroit et ne doivent pas
/// partager une ligne de journal.
fn appliquer_reponse_comprise(
    backend: &Arc<dyn DbBackend>,
    dev: &DiscoveredDlnaDevice,
    failure: &ReprobeFailure,
) {
    match failure {
        // L'appareil a REPONDU et n'expose plus de quoi jouer. Aucun reessai
        // n'y changera rien : l'entree part, sinon elle revient a chaque
        // demarrage.
        //
        // Un HEOS Denon/Marantz publie plusieurs identifiants UPnP pour un
        // seul appareil — media, lecteur, services. Tune les persistait tous :
        // chez Jean Valjean, CINQ entrees pour un materiel, chacune re-sondee
        // huit fois a chaque demarrage, soit 80 sondages voues a l'echec
        // (#1528). Quatre n'ont jamais ete des lecteurs et ne le deviendront
        // pas.
        ReprobeFailure::NotARenderer => warn!(
            uuid = %dev.uuid,
            nom = %dev.name,
            location = %dev.location,
            host = %dev.host,
            action = "l'appareil a répondu mais n'expose plus AVTransport : \
                      son entrée est retirée. S'il doit jouer, vérifiez son \
                      mode réseau/DLNA puis relancez une recherche.",
            "dlna_oublie_plus_un_lecteur"
        ),
        // Un AUTRE materiel occupe cette adresse — prouve par une cle stable,
        // pas suppose. Garder l'entree ferait piloter la mauvaise piece par la
        // zone du salon.
        ReprobeFailure::Replaced {
            observed_udn,
            observed_name,
            evidence,
        } => warn!(
            nom_attendu = %dev.name,
            nom_trouve = %observed_name,
            location = %dev.location,
            udn_memorise = %dev.uuid,
            udn_annonce = %observed_udn,
            distingue_par = evidence.label(),
            action = "un AUTRE appareil occupe désormais cette adresse : l'entrée \
                      mémorisée est retirée pour ne pas faire jouer la mauvaise \
                      pièce. Rallumez l'appareil attendu, puis relancez une \
                      recherche — il sera redécouvert.",
            "dlna_appareil_remplace_entree_retiree"
        ),
        // Ni preuve d'identite, ni preuve de substitution. On ne detruit rien.
        ReprobeFailure::IdentityUnclear { observed_udn } => warn!(
            nom = %dev.name,
            location = %dev.location,
            udn_memorise = %dev.uuid,
            udn_annonce = %observed_udn,
            action = "l'identifiant UPnP a changé et rien ne permet de distinguer \
                      « même appareil redémarré » de « autre appareil à la même \
                      adresse ». L'entrée est CONSERVÉE et sera re-sondée : aucune \
                      zone n'est perdue.",
            "dlna_identite_indecidable_entree_conservee"
        ),
        // #1280 — appareil fait taire par l'utilisateur. Deja journalise au
        // refus, et l'entree reste : elle est ce qui permettra au deblocage de
        // le faire revenir.
        ReprobeFailure::Ignore => {}
        // Filtre par l'appelant, qui gere le backoff. Rien a journaliser ici,
        // et `retire_l_entree()` rend `false` : le sens de defaut protege.
        ReprobeFailure::Unreachable(_) => {}
    }
    if failure.retire_l_entree() {
        forget_discovered_dlna(backend, &dev.uuid);
    }
}

/// Replie le magasin sur UNE entree par `LOCATION`, en gardant la premiere.
///
/// UPnP garantit qu'une `LOCATION` renvoie une seule description racine, donc
/// un seul appareil physique : deux entrees qui la partagent sont le meme
/// materiel vu par deux de ses UDN (racine, MediaRenderer, MediaServer,
/// ACT-Denon… pour un HEOS Denon/Marantz). Fonction pure, pour que la regle
/// soit testable sans reseau (#1703).
fn dedup_dlna_by_location(devices: Vec<DiscoveredDlnaDevice>) -> Vec<DiscoveredDlnaDevice> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    devices
        .into_iter()
        .filter(|d| seen.insert(d.location.clone()))
        .collect()
}

/// Retire un appareil du magasin des DLNA decouverts.
fn forget_discovered_dlna(backend: &Arc<dyn DbBackend>, uuid: &str) {
    let mut devices = load_discovered_dlna(backend);
    let before = devices.len();
    devices.retain(|d| d.uuid != uuid);
    if devices.len() != before {
        save_discovered_dlna(backend, &devices);
    }
}

/// Re-probe every persisted auto-discovered DLNA renderer at startup (mirrors
/// [`reregister_manual_devices`]). Each runs in its own task with backoff so a
/// briefly-unreachable device neither blocks the others nor delays boot.
pub async fn reprobe_persisted_dlna_devices(state: &AppState) {
    let stored = load_discovered_dlna(&state.backend);
    if stored.is_empty() {
        return;
    }
    // Les magasins ecrits avant #1703 contiennent une entree par UDN : chez
    // Jean Valjean, cinq pour un seul Marantz ND8006, toutes pointant sur
    // `http://…:60006/upnp/desc/aios_device/aios_device.xml`. On les replie
    // AVANT de sonder — sinon 5 x 8 tentatives = 80 sondages voues a l'echec
    // a chaque demarrage — et on reecrit le magasin pour qu'il guerisse.
    let stored_len = stored.len();
    let devices = dedup_dlna_by_location(stored);
    if devices.len() != stored_len {
        info!(
            before = stored_len,
            after = devices.len(),
            "discovered_dlna_collapsed_by_location"
        );
        save_discovered_dlna(&state.backend, &devices);
    }
    info!(count = devices.len(), "reprobing_discovered_dlna_devices");
    for dev in devices {
        let state = state.clone();
        tokio::spawn(async move { reprobe_dlna_with_backoff(&state, dev).await });
    }
}

async fn reprobe_dlna_with_backoff(state: &AppState, dev: DiscoveredDlnaDevice) {
    let mut delay = REREGISTER_BASE_DELAY;
    for attempt in 1..=REREGISTER_MAX_ATTEMPTS {
        match register_discovered_dlna(state, &dev).await {
            Ok(name) => {
                info!(uuid = %dev.uuid, name = %name, attempt, "discovered_dlna_reprobed");
                return;
            }
            Err(ReprobeFailure::Unreachable(e)) if attempt == REREGISTER_MAX_ATTEMPTS => {
                warn!(uuid = %dev.uuid, host = %dev.host, attempts = attempt, error = %e, "discovered_dlna_reprobe_gave_up");
                return;
            }
            Err(ReprobeFailure::Unreachable(e)) => {
                warn!(uuid = %dev.uuid, host = %dev.host, attempt, retry_in_s = delay.as_secs(), error = %e, "discovered_dlna_reprobe_retry");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(REREGISTER_MAX_DELAY);
            }
            // L'appareil a repondu et on a compris sa reponse : elle ne se
            // retente pas. Chaque cause a son message et sa consequence.
            Err(comprise) => {
                appliquer_reponse_comprise(&state.backend, &dev, &comprise);
                return;
            }
        }
    }
}

/// Probe a persisted DLNA renderer at its stored LOCATION and, if the
/// descriptor's UUID still matches, register the output + re-attach its zone.
async fn register_discovered_dlna(
    state: &AppState,
    dev: &DiscoveredDlnaDevice,
) -> Result<String, ReprobeFailure> {
    // #1280 — l'appareil a été fait taire depuis la dernière fois qu'il a été
    // vu. Le magasin `discovered_dlna` le re-sonderait à chaque démarrage et
    // le ré-enregistrerait : c'est l'un des chemins par lesquels il
    // « réapparaît rapidement ». Refus AVANT tout trafic réseau.
    if tune_core::db::ignored_device_repo::IgnoredDeviceRepo::with_backend(state.backend.clone())
        .is_ignored(
            tune_core::db::ignored_device_repo::DeviceIdentity::new(
                &dev.uuid, &dev.host, &dev.name,
            )
            .with_mac(Some(dev.mac.as_str())),
        )
    {
        info!(uuid = %dev.uuid, name = %dev.name, "discovered_dlna_appareil_ignore");
        return Err(ReprobeFailure::Ignore);
    }
    let desc = fetch_device_description(&dev.location).await.map_err(|e| {
        ReprobeFailure::Unreachable(format!(
            "cannot fetch DLNA description from {}: {e}",
            dev.location
        ))
    })?;
    // La DÉCOUVERTE accepte un appareil dont le `deviceType` n'est pas
    // MediaRenderer dès lors qu'il expose AVTransport (`ssdp.rs`,
    // « ssdp_non_standard_renderer_accepted ») — WiiM, foobar2000, et les
    // enveloppes HEOS. Cette re-vérification, elle, exigeait le type standard.
    //
    // Les deux se contredisaient, et le résultat était pire qu'un simple
    // refus : l'appareil était découvert, enregistré, PUIS oublié quelques
    // minutes plus tard par le re-sondage. Chez Jean Valjean (#1879), le
    // Marantz ND8006 disparaissait ainsi à chaque démarrage depuis la 0.9.81 —
    // son enveloppe AiOS se déclare `MediaServer:1` alors qu'elle porte bien
    // AVTransport, et l'unification par LOCATION (#1791) a fait de cette
    // description racine la seule retenue.
    //
    // On aligne donc la tolérance sur celle de la découverte. La sévérité
    // utile n'est pas perdue : le contrôle ci-dessous exige AVTransport ET
    // RenderingControl, donc un appareil qui n'est réellement pas un lecteur
    // est toujours écarté — et le reste des frères HEOS, qui n'exposent aucun
    // de ces services, continue d'être oublié comme le voulait #1528.
    if !desc.is_media_renderer() && !desc.has_av_transport() {
        return Err(ReprobeFailure::NotARenderer);
    }
    // ── Un UDN qui change n'est PAS une preuve de substitution (#2639) ─────
    //
    // La garde d'origine (« only re-attach when the descriptor's UUID still
    // matches ») protège d'un vrai risque : le bail DHCP expire, un AUTRE
    // appareil hérite de l'adresse, et la zone du salon se met à piloter
    // l'ampli de la chambre. Mais un UDN se régénère aussi tout seul — un
    // Marantz ND8006 a changé le sien entre le 22 et le 28/08/2026 à LOCATION
    // inchangée, pendant que son SSDP annonçait encore l'ancien. La garde
    // effaçait alors l'entrée, donc la zone, sous un message qui annonçait
    // « pas un lecteur ».
    //
    // On demande donc à une clé stable de trancher. La MAC est relevée
    // maintenant : le `fetch` ci-dessus vient d'échanger du TCP avec l'hôte,
    // le cache ARP est chaud — c'est exactement ce que fait déjà
    // `build_renderer_device`. Une absence n'accuse personne (cf.
    // `renderer_identity`).
    let observed_mac = tune_core::discovery::mac::arp_lookup(&dev.host).unwrap_or_default();
    let observed = RendererIdentity {
        udn: &desc.udn,
        mac: &observed_mac,
        friendly_name: &desc.friendly_name,
        manufacturer: &desc.manufacturer,
        model_name: &desc.model_name,
    };
    // Précondition de `compare_at_same_location` : `desc` sort de
    // `fetch_device_description(&dev.location)`, donc de l'URL persistée mot
    // pour mot — même IP, même port, même chemin.
    match compare_at_same_location(dev.identity(), observed) {
        IdentityVerdict::NoDisagreement => {}
        IdentityVerdict::SameHardware(evidence) => {
            info!(
                nom = %desc.friendly_name,
                location = %dev.location,
                udn_memorise = %dev.uuid,
                udn_annonce = %desc.udn,
                reconnu_par = evidence.label(),
                action = "l'appareil a changé d'identifiant UPnP (redémarrage, mise à jour \
                          ou réinitialisation). Il reste reconnu : sa zone est conservée, \
                          rien à faire.",
                "dlna_identifiant_change_appareil_reconnu"
            );
        }
        IdentityVerdict::OtherHardware(evidence) => {
            return Err(ReprobeFailure::Replaced {
                observed_udn: desc.udn.clone(),
                observed_name: desc.friendly_name.clone(),
                evidence,
            });
        }
        IdentityVerdict::Undecidable => {
            return Err(ReprobeFailure::IdentityUnclear {
                observed_udn: desc.udn.clone(),
            });
        }
    }
    let service_urls = desc.service_urls();
    let (Some(av), Some(rc)) = (
        service_urls.get("avtransport"),
        service_urls.get("renderingcontrol"),
    ) else {
        return Err(ReprobeFailure::NotARenderer);
    };
    let base = format!("http://{}:{}", dev.host, dev.port);
    let device_name = if dev.name.is_empty() {
        format!("DLNA {}", dev.host)
    } else {
        dev.name.clone()
    };
    let delay =
        crate::config::resolve_play_delay(&state.backend, &state.config, &dev.uuid, &device_name);
    let cm_url = service_urls
        .get("connectionmanager")
        .or_else(|| service_urls.get("ConnectionManager"))
        .map(|p| format!("{base}{p}"));
    let dlna = DlnaOutput::new(
        device_name.clone(),
        dev.uuid.clone(),
        dev.host.clone(),
        format!("{base}{av}"),
        format!("{base}{rc}"),
        cm_url,
    )
    .with_play_delay(delay);
    // Registry is keyed by device_id (the uuid): a later multicast discovery
    // replaces this entry rather than duplicating it.
    state.outputs.lock().await.register(Box::new(dlna));
    let _ = ensure_zone(state, &device_name, "dlna", &dev.uuid);
    // Drive auto_resume: it waits on `device.reconnected` to resume a zone that
    // was playing before the restart — the multicast path may never fire for a
    // lazy SSDP responder, which is the whole point of #1126.
    state.event_bus.emit(
        "device.reconnected",
        json!({ "device_id": &dev.uuid, "name": &device_name, "host": &dev.host }),
    );
    // Le magasin apprend les cles de reconnaissance relevees a l'instant (#2639).
    // C'est ce qui fait qu'un magasin ancien ne reste pas eternellement au
    // niveau le plus faible : au prochain desaccord d'UDN, la MAC ou le couple
    // marque/modele trancheront a la place du seul nom. On garde `dev.uuid`
    // comme cle : c'est l'ancre de la zone, et le SSDP de cet appareil continue
    // de l'annoncer.
    persist_discovered_dlna(
        &state.backend,
        &DiscoveredDlnaDevice::new(&dev.uuid, &dev.location, &device_name, &dev.host, dev.port)
            .with_identity(&observed_mac, &desc.manufacturer, &desc.model_name),
    );
    Ok(device_name)
}

async fn add_device(
    State(state): State<AppState>,
    Json(body): Json<AddDeviceRequest>,
) -> impl IntoResponse {
    let device_type = body.r#type.to_lowercase();
    let host = body.host.trim().to_string();

    if host.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "host is required"})),
        )
            .into_response();
    }

    if device_type != "dlna" && device_type != "bluos" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Unsupported device type: '{}'. Supported: bluos, dlna", device_type),
            })),
        )
            .into_response();
    }

    let default_port = if device_type == "bluos" { 11000 } else { 80 };
    let dev = ManualDevice {
        r#type: device_type,
        host,
        port: body.port.unwrap_or(default_port),
        name: body.name,
    };

    match register_manual_device(&state, &dev).await {
        Ok((device_id, name, zone_id)) => {
            persist_manual_device(&state, &dev);
            (
                StatusCode::CREATED,
                Json(json!({
                    "status": "ok",
                    "device_id": device_id,
                    "name": name,
                    "type": dev.r#type,
                    "host": dev.host,
                    "port": dev.port,
                    "zone_id": zone_id,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": e,
                "hint": "Verify the IP address and port, and that the device is powered on.",
            })),
        )
            .into_response(),
    }
}

/// Extract a tag value from XML (simple, non-recursive).
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let text = xml[start..end].trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

async fn scan_devices(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let devices = scanner.rescan().await;

    // #1280 — les appareils que l'utilisateur a fait taire ne sont ni
    // enregistrés, ni annoncés, ni listés par ce scan.
    let deduped: Vec<_> = dedup_devices(devices)
        .into_iter()
        .filter(|d| !crate::discovery_setup::appareil_ignore(&state.backend, d))
        .collect();

    let mut registered = 0;
    {
        let mut outputs = state.outputs.lock().await;
        for d in &deduped {
            let location = d.location.as_deref().unwrap_or("");
            if location.is_empty() {
                continue;
            }

            if let Ok(desc) = fetch_device_description(location).await
                && desc.is_media_renderer()
            {
                let service_urls = desc.service_urls();
                let av_url = service_urls.get("avtransport");
                let rc_url = service_urls.get("renderingcontrol");

                if let (Some(av), Some(rc)) = (av_url, rc_url) {
                    let base = format!("http://{}:{}", d.host, d.port);
                    let delay = crate::config::resolve_play_delay(
                        &state.backend,
                        &state.config,
                        &d.id,
                        &d.name,
                    );
                    let cm_url = service_urls
                        .get("connectionmanager")
                        .or_else(|| service_urls.get("ConnectionManager"))
                        .map(|p| format!("{base}{p}"));
                    let dlna = DlnaOutput::new(
                        d.name.clone(),
                        d.id.clone(),
                        d.host.clone(),
                        format!("{base}{av}"),
                        format!("{base}{rc}"),
                        cm_url,
                    )
                    .with_play_delay(delay);
                    outputs.register(Box::new(dlna));
                    registered += 1;
                }
            }
        }
    }

    // Emit device.discovered for each found device
    for d in &deduped {
        state.event_bus.emit(
            "device.discovered",
            json!({
                "id": &d.id,
                "name": &d.name,
                "host": &d.host,
                "type": format!("{:?}", d.device_type),
            }),
        );
    }

    let items: Vec<Value> = deduped
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "type": format!("{:?}", d.device_type),
                "host": d.host,
                "port": d.port,
                "available": d.available,
                "manufacturer": d.manufacturer,
                "model": d.model,
            })
        })
        .collect();

    Json(json!({
        "items": items,
        "total": items.len(),
        "dlna_outputs_registered": registered,
    }))
}

async fn list_audio_devices(State(state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let backend = &state.display_audio_backend();
        // The web client's sidebar fetches this on every page load. Re-enumerating
        // WASAPI devices probes each device's formats, which can crash the active
        // render stream and stop playback on Windows (DEvir: refresh UI during
        // local playback → audio dies). While a local output is playing, serve the
        // last cached device list instead of re-scanning the hardware.
        let devices = if crate::background::any_local_output_playing(&state).await {
            tune_core::outputs::local::cached_audio_devices()
        } else {
            tune_core::outputs::local::list_audio_devices_with_backend(backend)
        };
        // Publier l'identifiant de registre à côté du nom.
        //
        // Cette charge utile n'en portait aucun, et un client qui veut créer
        // une zone n'a pas d'autre choix que de deviner. Le panneau latéral du
        // client web devinait « le nom », d'où des zones sans le préfixe
        // `local:` — invisibles au dédoublonnage, et traitées par
        // l'orchestrateur comme des renderers réseau, ce qui bloquait la
        // lecture plus d'une minute avant de ne rien jouer (DEvir, #1823).
        //
        // La clé est celle que posent les quatre points d'enregistrement
        // (`local.rs`, `background.rs`, `startup.rs`) : `local:{name}`.
        let devices: Vec<Value> = devices
            .into_iter()
            .map(|d| {
                let mut v = serde_json::to_value(&d).unwrap_or_else(|_| json!({}));
                if let Some(o) = v.as_object_mut() {
                    o.insert("id".into(), json!(format!("local:{}", d.name)));
                }
                v
            })
            .collect();
        // #1395 — l'écran qui porte le sélecteur de backend est le premier
        // endroit où l'écart doit se lire : Bilou choisit ASIO ici, la lecture
        // sort en WASAPI, et rien ne le lui dit. `backend` (l'actif) ne suffit
        // pas — sans le demandé à côté, il ne peut pas savoir si c'est son
        // réglage qui n'a pas pris ou le serveur qui a basculé.
        //
        // Additif : `backend` garde exactement sa valeur d'avant.
        let backend_status = tune_core::outputs::local::active_backend_status(backend);
        Json(json!({
            "devices": devices,
            "backend": tune_core::outputs::local::active_backend_name(backend),
            "backend_status": backend_status,
            "asio_available": tune_core::outputs::local::asio_available(),
            // #1268 — la liste des backends que le sélecteur peut proposer,
            // filtrée par la plateforme du SERVEUR. Le client l'écrivait en
            // dur (Auto/WASAPI/ASIO), et des machines Debian/Fedora se
            // voyaient offrir deux technologies Windows.
            "supported_backends": tune_core::outputs::local::supported_backends(),
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        let _ = state;
        Json(json!({
            "devices": [],
            "backend": "none",
            "backend_status": Value::Null,
            "asio_available": false,
            "supported_backends": [],
        }))
    }
}

/// List ASIO audio devices (Windows-only, requires `asio` feature).
///
/// Returns ASIO driver names, supported sample rates, and channel counts.
/// On non-Windows platforms or without the `asio` feature, returns an empty
/// list with `asio_available: false`.
async fn list_asio_devices(State(_state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let devices = tokio::task::spawn_blocking(tune_core::outputs::local::list_asio_devices)
            .await
            .unwrap_or_default();
        Json(json!({
            "devices": devices,
            "asio_available": tune_core::outputs::local::asio_available(),
            "count": devices.len(),
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        Json(json!({
            "devices": [],
            "asio_available": false,
            "count": 0,
        }))
    }
}

/// Trigger immediate re-enumeration of local audio devices (USB DAC hot-plug).
async fn rescan_local_devices(State(state): State<AppState>) -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        crate::background::rescan_local_audio_devices(&state).await;
        let outputs = state.outputs.lock().await;
        let local_devices: Vec<String> = outputs
            .list()
            .into_iter()
            .filter(|id| id.starts_with("local:"))
            .collect();
        Json(json!({
            "status": "ok",
            "local_devices": local_devices.len(),
            "devices": local_devices,
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        let _ = state;
        Json(json!({
            "status": "unsupported",
            "message": "local-audio feature not enabled",
        }))
    }
}

async fn device_status(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&device_id) else {
        return (StatusCode::NOT_FOUND, "device not found").into_response();
    };
    let output = output.lock().await;
    match output.get_status().await {
        Ok(status) => Json(json!(status)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

// --- Device buffer stats ---

fn buffer_settings_for(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    device_id: &str,
) -> (f64, bool) {
    let settings = SettingsRepo::with_backend(backend.clone());
    let key = format!("buffer_{device_id}");
    if let Ok(Some(val)) = settings.get(&key) {
        if let Ok(obj) = serde_json::from_str::<Value>(&val) {
            let buf = obj.get("buffer_s").and_then(|v| v.as_f64()).unwrap_or(2.0);
            let auto = obj.get("auto").and_then(|v| v.as_bool()).unwrap_or(true);
            return (buf, auto);
        }
    }
    (2.0, true)
}

async fn all_buffer_stats(State(state): State<AppState>) -> Json<Value> {
    let outputs = state.outputs.lock().await;
    let device_ids = outputs.list();
    let mut stats = Vec::new();
    for device_id in &device_ids {
        if let Some(output) = outputs.get(device_id) {
            let output = output.lock().await;
            let (buffer_s, auto) = buffer_settings_for(&state.backend, device_id);
            stats.push(json!({
                "device_id": device_id,
                "device_name": output.name(),
                "buffer_s": buffer_s,
                "auto": auto,
                "manual_override": !auto,
                "total_disconnections": 0,
                "total_underruns": 0,
            }));
        }
    }
    Json(json!(stats))
}

async fn device_buffer_stats(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let Some(output) = outputs.get(&device_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "device not found"})),
        )
            .into_response();
    };
    let output = output.lock().await;
    let (buffer_s, auto) = buffer_settings_for(&state.backend, &device_id);
    Json(json!({
        "device_id": device_id,
        "device_name": output.name(),
        "buffer_s": buffer_s,
        "auto": auto,
        "manual_override": !auto,
        "total_disconnections": 0,
        "total_underruns": 0,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct BufferSettings {
    buffer_s: Option<f64>,
    auto: Option<bool>,
}

async fn set_device_buffer(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<BufferSettings>,
) -> impl IntoResponse {
    // Verify device exists
    {
        let outputs = state.outputs.lock().await;
        if outputs.get(&device_id).is_none() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "device not found"})),
            )
                .into_response();
        }
    }

    let (current_buf, current_auto) = buffer_settings_for(&state.backend, &device_id);
    let new_buf = body.buffer_s.unwrap_or(current_buf);
    let new_auto = body.auto.unwrap_or(current_auto);

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("buffer_{device_id}");
    let val = json!({"buffer_s": new_buf, "auto": new_auto}).to_string();
    if let Err(e) = settings.set(&key, &val) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    Json(json!({
        "device_id": device_id,
        "buffer_s": new_buf,
        "auto": new_auto,
        "manual_override": !new_auto,
    }))
    .into_response()
}

async fn clear_devices(State(state): State<AppState>) -> impl IntoResponse {
    let outputs = state.outputs.lock().await;
    let ids: Vec<String> = outputs.list();
    drop(outputs);
    let mut removed = 0;
    for id in ids {
        let mut outputs = state.outputs.lock().await;
        outputs.remove(&id);
        removed += 1;
    }
    // Forget all persisted manual devices too, so a clear is durable.
    save_manual_devices(&state, &[]);
    Json(json!({"cleared": removed}))
}

async fn delete_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let mut outputs = state.outputs.lock().await;
    outputs.remove(&device_id);
    drop(outputs);
    // Also drop it from persistence so it isn't re-registered on next startup.
    forget_manual_device(&state, &device_id);
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::DeviceLost,
        json!({ "device_id": device_id }),
    );
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// Appareils ignorés (#1280)
// ---------------------------------------------------------------------------

/// L'instantané d'identité à figer quand l'utilisateur fait taire un appareil.
///
/// Trois sources, dans l'ordre de fiabilité — la première qui répond gagne :
///
/// 1. **le scanner vivant**, y compris les identités secondaires repliées dans
///    `capabilities.alternatives` : c'est là qu'on trouve la MAC, donc la seule
///    clé qui survive à un changement d'adresse ;
/// 2. **le registre des sorties**, quand le scanner est entre deux passes ;
/// 3. **la zone persistée** portant cet identifiant — le geste peut venir de la
///    liste des zones, où l'appareil n'est plus annoncé.
///
/// Si aucune ne répond, l'instantané se réduit à l'identifiant : l'identité
/// exacte reste bloquée, ce qui est déjà le comportement attendu.
async fn instantane_d_identite(
    state: &AppState,
    device_id: &str,
) -> tune_core::db::ignored_device_repo::IgnoredDevice {
    let mut snapshot = tune_core::db::ignored_device_repo::IgnoredDevice {
        device_id: device_id.to_string(),
        mac: String::new(),
        host: String::new(),
        name: String::new(),
        device_type: String::new(),
        created_at: None,
    };

    // 1. Le scanner vivant — identités secondaires comprises.
    let vivants = state.scanner.devices().await;
    let vu = vivants.iter().find(|d| {
        d.id == device_id
            || d.capabilities
                .get("alternatives")
                .and_then(|a| a.as_array())
                .is_some_and(|alts| {
                    alts.iter()
                        .filter_map(|a| a.get("id")?.as_str())
                        .any(|id| id == device_id)
                })
    });
    if let Some(d) = vu {
        snapshot.mac = d.mac_address.clone().unwrap_or_default();
        snapshot.host.clone_from(&d.host);
        snapshot.name.clone_from(&d.name);
        snapshot.device_type = d.device_type.to_string();
        return snapshot;
    }

    // 2. Le registre des sorties.
    let infos = state.outputs.lock().await.info_all().await;
    if let Some(info) = infos
        .iter()
        .find(|i| i.get("device_id").and_then(Value::as_str) == Some(device_id))
    {
        let s = |cle: &str| {
            info.get(cle)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        snapshot.host = s("host");
        snapshot.name = s("name");
        snapshot.device_type = s("type");
        return snapshot;
    }

    // 3. La zone persistée.
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    if let Ok(Some(zone)) = zone_repo.get_by_device_id(device_id) {
        snapshot.name = zone.name;
        snapshot.device_type = zone.output_type.unwrap_or_default();
    }
    snapshot
}

/// `POST /devices/{device_id}/ignore` — faire taire un APPAREIL.
///
/// Trois effets, et les trois comptent : l'identité est figée (l'appareil ne
/// revient plus à aucun scan, sous aucune de ses identités), sa sortie quitte
/// le registre (il cesse d'être proposé TOUT DE SUITE, sans attendre un
/// redémarrage), et sa zone est masquée si elle existe (sinon la zone
/// resterait jouable alors que l'appareil est censé s'être tu).
async fn ignore_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    if device_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "device_id"}))).into_response();
    }
    let snapshot = instantane_d_identite(&state, &device_id).await;
    let repo =
        tune_core::db::ignored_device_repo::IgnoredDeviceRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.ignore(&snapshot) {
        warn!(device_id = %device_id, error = %e, "device_ignore_failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    // La sortie quitte le registre : sans cela l'appareil resterait proposé
    // par `GET /devices` jusqu'au prochain redémarrage — exactement le
    // « il revient » du ticket, à peine déplacé.
    let infos = {
        let outputs = state.outputs.lock().await;
        outputs.info_all().await
    };
    {
        let mut outputs = state.outputs.lock().await;
        outputs.remove(&device_id);
        // Les identités JUMELLES du même appareil, enregistrées sous un autre
        // identifiant : c'est par elles que le Sonos revenait.
        for info in &infos {
            let Some(id) = info.get("device_id").and_then(Value::as_str) else {
                continue;
            };
            if id == device_id {
                continue;
            }
            let s = |cle: &str| info.get(cle).and_then(Value::as_str).unwrap_or_default();
            let identite =
                tune_core::db::ignored_device_repo::DeviceIdentity::new(id, s("host"), s("name"));
            if tune_core::db::ignored_device_repo::identity_matches(&snapshot, identite) {
                outputs.remove(id);
            }
        }
    }
    forget_manual_device(&state, &device_id);

    // La zone de l'appareil, s'il en a une : masquée, comme une suppression.
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let mut zones_masquees = Vec::new();
    if let Ok(Some(zone)) = zone_repo.get_by_device_id(&device_id)
        && let Some(zid) = zone.id
        && zone_repo.delete(zid).is_ok()
    {
        zones_masquees.push(zid);
    }
    if !snapshot.host.is_empty() || !snapshot.mac.is_empty() {
        let mac = (!snapshot.mac.is_empty()).then_some(snapshot.mac.as_str());
        // Masquer une zone la retire des zones VISIBLES, donc l'appel suivant
        // rend la jumelle. Borne dure quand même : on ne fait pas dépendre la
        // terminaison d'une écriture en base.
        for _ in 0..16 {
            let Some((zid, _, _)) = zone_repo.find_visible_zone_by_identity(&snapshot.host, mac)
            else {
                break;
            };
            if zones_masquees.contains(&zid) || zone_repo.delete(zid).is_err() {
                break;
            }
            zones_masquees.push(zid);
        }
    }

    state.event_bus.emit_typed(
        tune_core::event_types::EventType::DeviceLost,
        json!({ "device_id": device_id, "reason": "ignored" }),
    );
    info!(
        device_id = %device_id,
        name = %snapshot.name,
        host = %snapshot.host,
        zones = zones_masquees.len(),
        "device_ignored"
    );
    Json(json!({
        "ignored": snapshot,
        "hidden_zone_ids": zones_masquees,
    }))
    .into_response()
}

/// `DELETE /devices/{device_id}/ignore` — débloquer.
///
/// Libère TOUTES les identités du même appareil : sans cela, l'utilisateur qui
/// a fait taire son Sonos devrait deviner l'UUID jumeau pour le récupérer.
/// Idempotent : débloquer ce qui ne l'était pas rend `released: []`.
async fn unignore_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let repo =
        tune_core::db::ignored_device_repo::IgnoredDeviceRepo::with_backend(state.backend.clone());
    match repo.unignore(&device_id) {
        Ok(liberes) => {
            info!(device_id = %device_id, released = liberes.len(), "device_unignored");
            // L'appareil ne revient qu'au prochain passage de découverte —
            // l'événement fait recharger Réglages > Réseau, où il réapparaîtra.
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::DeviceDiscovered,
                json!({ "device_id": device_id, "reason": "unignored" }),
            );
            Json(json!({ "released": liberes })).into_response()
        }
        Err(e) => {
            warn!(device_id = %device_id, error = %e, "device_unignore_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
        }
    }
}

/// `GET /devices/ignored` — la liste de révision.
///
/// Elle rend l'instantané figé, pas l'appareil vivant : un appareil ignoré
/// n'est plus annoncé nulle part, donc c'est la SEULE vue depuis laquelle il
/// peut être débloqué.
async fn list_ignored_devices(State(state): State<AppState>) -> impl IntoResponse {
    let repo =
        tune_core::db::ignored_device_repo::IgnoredDeviceRepo::with_backend(state.backend.clone());
    match repo.list() {
        Ok(items) => Json(json!({ "total": items.len(), "items": items })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Device Pairing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PairRequest {
    friendly_name: Option<String>,
}

async fn pair_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<PairRequest>,
) -> impl IntoResponse {
    // Check if this is an AirPlay 2 device — trigger PIN display
    let is_airplay2 = device_id.starts_with("airplay2:");
    let host = if is_airplay2 {
        let outputs = state.outputs.lock().await;
        if let Some(arc) = outputs.get(&device_id) {
            let o = arc.lock().await;
            o.host().map(|h| h.to_string())
        } else {
            None
        }
    } else {
        None
    };

    if is_airplay2 {
        if let Some(host) = host {
            let url = format!("http://{}:7000/pair-pin-start", host);
            let client = tune_core::http::client::shared();
            match client
                .post(&url)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!(device = %device_id, "airplay2_pair_pin_start_triggered");
                    return Json(json!({
                        "status": "awaiting_pin",
                        "device_id": device_id,
                        "message": "Enter the 4-digit PIN shown on the device screen",
                    }))
                    .into_response();
                }
                Ok(resp) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": format!("device returned HTTP {}", resp.status()),
                        })),
                    )
                        .into_response();
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": format!("failed to reach device: {e}"),
                        })),
                    )
                        .into_response();
                }
            }
        }
    }

    // Non-AirPlay 2: simple pair registration
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("device_pair_{device_id}");
    let data = json!({
        "device_id": device_id,
        "friendly_name": body.friendly_name,
        "paired_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "status": "paired",
    });
    settings.set(&key, &data.to_string()).ok();
    (StatusCode::CREATED, Json(data)).into_response()
}

#[derive(Deserialize)]
struct PairPinRequest {
    pin: String,
}

async fn pair_device_pin(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<PairPinRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Check if there's a pending pin
    let pending_key = format!("device_pair_pin_{device_id}");
    let expected = settings.get(&pending_key).ok().flatten();
    if let Some(ref expected_pin) = expected {
        if expected_pin != &body.pin {
            return (StatusCode::FORBIDDEN, Json(json!({"error": "invalid PIN"}))).into_response();
        }
    }
    // Mark device as paired
    let key = format!("device_pair_{device_id}");
    let data = json!({
        "device_id": device_id,
        "paired_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "status": "paired",
        "pin_verified": true,
    });
    settings.set(&key, &data.to_string()).ok();
    settings.delete(&pending_key).ok();
    Json(data).into_response()
}

/// Trigger AirPlay 2 PIN display on an Apple TV.
/// The device shows a 4-digit PIN that the user enters via POST /pair/pin.
async fn airplay2_pair_pin_start(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    // Find the device's host from the output registry
    let host = {
        let outputs = state.outputs.lock().await;
        if let Some(arc) = outputs.get(&device_id) {
            let o = arc.lock().await;
            o.host().map(|h| h.to_string())
        } else {
            None
        }
    };
    let host = match host {
        Some(h) => h,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "device not found or not connected"})),
            )
                .into_response();
        }
    };
    let port = 7000u16;

    // POST /pair-pin-start to the Apple TV
    let url = format!("http://{}:{}/pair-pin-start", host, port);
    let client = tune_core::http::client::shared();
    match client
        .post(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(device = %device_id, host = %host, "airplay2_pair_pin_start_sent");
            Json(json!({
                "status": "pin_requested",
                "device_id": device_id,
                "message": "Check the device screen for a 4-digit PIN",
            }))
            .into_response()
        }
        Ok(resp) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("device returned HTTP {}", resp.status()),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("failed to reach device: {e}"),
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod dlna_reprobe_tests {
    use super::*;

    // ── Rejet definitif contre panne passagere (#1528) ────────────────────
    //
    // Un HEOS Denon/Marantz publie plusieurs identifiants UPnP pour un seul
    // appareil. Tune les persistait tous : chez Jean Valjean, CINQ entrees
    // pour un materiel, chacune re-sondee huit fois a chaque demarrage.
    // Quatre n'ont jamais ete des lecteurs et ne le deviendront pas.

    /// Une panne d'accès reste ré-essayable, et elle SEULE.
    ///
    /// Le sens de défaut du re-sondage : on ne retire une entrée que sur une
    /// réponse qu'on a explicitement comprise. Avant #2639 cette distinction
    /// se faisait par sous-chaîne (`DEFINITIVE_MARKERS`) ; elle est portée par
    /// le type, donc le compilateur.
    #[test]
    fn seul_un_echec_d_acces_est_reessayable() {
        let injoignable = ReprobeFailure::Unreachable(
            "cannot fetch DLNA description from http://192.168.1.11:60006/d.xml: timed out".into(),
        );
        assert!(matches!(injoignable, ReprobeFailure::Unreachable(_)));

        // Les trois autres causes ne se retentent jamais : deux retirent
        // l'entrée, la troisième la conserve — mais aucune ne boucle.
        for cause in [
            ReprobeFailure::NotARenderer,
            ReprobeFailure::Replaced {
                observed_udn: "uuid:b".into(),
                observed_name: "Ampli chambre".into(),
                evidence: Evidence::Mac,
            },
            ReprobeFailure::IdentityUnclear {
                observed_udn: "uuid:b".into(),
            },
        ] {
            assert!(
                !matches!(cause, ReprobeFailure::Unreachable(_)),
                "une reponse comprise ne doit pas repartir dans le backoff : {cause:?}"
            );
        }
    }

    /// Les deux chemins doivent appliquer la MÊME tolérance.
    ///
    /// La découverte accepte un appareil au `deviceType` non standard s'il
    /// expose AVTransport ; la re-vérification l'exigeait strictement
    /// MediaRenderer. L'appareil était donc découvert, enregistré, puis oublié
    /// quelques minutes plus tard — le Marantz ND8006 de Jean Valjean, dont
    /// l'enveloppe AiOS se déclare `MediaServer:1` (#1879).
    ///
    /// Ce test compare les deux prédicats sur les trois descriptions qui
    /// comptent : un lecteur standard, une enveloppe non standard qui porte
    /// AVTransport, et un vrai non-lecteur.
    #[test]
    fn la_reverification_tolere_ce_que_la_decouverte_accepte() {
        use tune_core::discovery::xml_parser::{DeviceDescription, ServiceDescription};

        let svc = |t: &str| ServiceDescription {
            service_type: t.to_string(),
            control_url: "/ctrl".to_string(),
            ..Default::default()
        };
        let desc = |device_type: &str, services: Vec<ServiceDescription>| DeviceDescription {
            device_type: device_type.to_string(),
            friendly_name: "x".to_string(),
            udn: "uuid:1".to_string(),
            services,
            ..Default::default()
        };

        // Un lecteur standard : accepté des deux côtés, hier comme aujourd'hui.
        let standard = desc(
            "urn:schemas-upnp-org:device:MediaRenderer:1",
            vec![svc("urn:schemas-upnp-org:service:AVTransport:1")],
        );
        assert!(standard.is_media_renderer());

        // L'enveloppe AiOS du Marantz : type MediaServer, mais AVTransport
        // présent. La découverte l'accepte — la re-vérification doit suivre.
        let aios = desc(
            "urn:schemas-upnp-org:device:MediaServer:1",
            vec![svc("urn:schemas-upnp-org:service:AVTransport:1")],
        );
        assert!(!aios.is_media_renderer(), "le type reste non standard");
        assert!(
            aios.has_av_transport(),
            "c'est ce service qui le rend jouable, et que la découverte regarde"
        );

        // Un frère HEOS sans AVTransport : refusé des deux côtés. C'est ce qui
        // empêche les cinq entrées de #1528 de revenir.
        let frere = desc(
            "urn:schemas-upnp-org:device:MediaServer:1",
            vec![svc("urn:schemas-upnp-org:service:ContentDirectory:1")],
        );
        assert!(!frere.is_media_renderer());
        assert!(
            !frere.has_av_transport(),
            "sans AVTransport, l'entrée doit continuer d'être oubliée"
        );
    }

    // ── Le verdict d'identite, applique au magasin (#2639) ────────────────
    //
    // `compare_at_same_location` est teste sur pieces dans
    // `tune-core/src/discovery/renderer_identity.rs`. Ici on verifie ce que
    // l'entree PERSISTEE devient — c'est-a-dire si la zone survit.

    /// L'entree telle que le magasin de Jean Valjean la porte : pas de MAC,
    /// pas de marque, pas de modele — le format d'avant #2639.
    fn entree_marantz(backend: &Arc<dyn DbBackend>) {
        persist_discovered_dlna(
            backend,
            &DiscoveredDlnaDevice::new(
                "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26",
                AIOS_LOCATION,
                "Marantz ND8006",
                "192.168.1.11",
                60006,
            ),
        );
    }

    /// Ce que le descripteur du Marantz rendait le 28/08/2026 : UDN regenere,
    /// tout le reste inchange.
    fn marantz_apres_bascule() -> RendererIdentity<'static> {
        RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "",
            friendly_name: "Marantz ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        }
    }

    /// La table qui decide si l'utilisateur perd sa zone.
    ///
    /// C'est le defaut de #2639 en une assertion : avant, `Replaced` ET
    /// `IdentityUnclear` retiraient tous deux l'entree, parce que les deux
    /// n'existaient pas — un seul `Err(String)` couvrait les deux, sous un
    /// message qui en annoncait une troisieme.
    #[test]
    fn on_ne_retire_une_entree_que_sur_une_preuve() {
        assert!(
            ReprobeFailure::NotARenderer.retire_l_entree(),
            "l'appareil a repondu et n'est plus un lecteur : l'entree part"
        );
        assert!(
            ReprobeFailure::Replaced {
                observed_udn: "uuid:c0bfdbad".into(),
                observed_name: "Ampli chambre".into(),
                evidence: Evidence::Mac,
            }
            .retire_l_entree(),
            "une cle stable designe un AUTRE materiel : l'entree part"
        );
        assert!(
            !ReprobeFailure::IdentityUnclear {
                observed_udn: "uuid:c0bfdbad".into(),
            }
            .retire_l_entree(),
            "un desaccord d'UDN sans preuve ne doit RIEN retirer : c'est la \
             zone de l'utilisateur qui partirait avec"
        );
        assert!(
            !ReprobeFailure::Unreachable("timed out".into()).retire_l_entree(),
            "un appareil eteint garde sa place"
        );
    }

    /// Bout en bout sur le magasin : l'entree du Marantz survit a un UDN
    /// regenere, et ne survit PAS a une substitution prouvee.
    #[test]
    fn la_table_appliquee_au_magasin_garde_ou_retire_la_bonne_entree() {
        for (cause, doit_rester) in [
            (
                ReprobeFailure::IdentityUnclear {
                    observed_udn: "uuid:c0bfdbad".into(),
                },
                true,
            ),
            (
                ReprobeFailure::Replaced {
                    observed_udn: "uuid:c0bfdbad".into(),
                    observed_name: "Ampli chambre".into(),
                    evidence: Evidence::FriendlyName,
                },
                false,
            ),
            (ReprobeFailure::NotARenderer, false),
            (ReprobeFailure::Unreachable("timed out".into()), true),
        ] {
            let backend = memory_backend();
            entree_marantz(&backend);
            let dev = load_discovered_dlna(&backend).remove(0);
            appliquer_reponse_comprise(&backend, &dev, &cause);
            assert_eq!(
                !load_discovered_dlna(&backend).is_empty(),
                doit_rester,
                "magasin apres {cause:?}"
            );
        }
    }

    #[test]
    fn un_udn_regenere_ne_fait_pas_disparaitre_la_zone() {
        let backend = memory_backend();
        entree_marantz(&backend);
        let stored = load_discovered_dlna(&backend);

        let verdict = compare_at_same_location(stored[0].identity(), marantz_apres_bascule());
        assert!(
            matches!(verdict, IdentityVerdict::SameHardware(_)),
            "le Marantz doit rester reconnu, or le verdict est {verdict:?}"
        );

        // Le verdict « meme materiel » ne passe par AUCUNE branche qui efface.
        // Contre-epreuve directe : l'entree est toujours la, sous son UUID
        // d'origine — l'ancre de la zone.
        assert_eq!(load_discovered_dlna(&backend).len(), 1);
        assert_eq!(
            load_discovered_dlna(&backend)[0].uuid,
            "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26",
            "la cle de zone doit rester la valeur persistee, pas l'UDN du descripteur"
        );
    }

    #[test]
    fn un_autre_appareil_a_la_meme_adresse_fait_bien_retirer_l_entree() {
        // Le piege symetrique : la garde d'origine existait pour CA, et elle
        // doit continuer de fonctionner.
        let backend = memory_backend();
        entree_marantz(&backend);
        let stored = load_discovered_dlna(&backend);

        let intrus = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "",
            friendly_name: "Ampli chambre",
            manufacturer: "Denon",
            model_name: "AVR-X2700H",
        };
        assert!(
            matches!(
                compare_at_same_location(stored[0].identity(), intrus),
                IdentityVerdict::OtherHardware(_)
            ),
            "un autre appareil ne doit jamais heriter de la zone du Marantz"
        );

        // C'est la branche `Replaced` qui appelle `forget_discovered_dlna` :
        // on verifie que ce retrait fait bien disparaitre l'entree.
        forget_discovered_dlna(&backend, "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26");
        assert!(load_discovered_dlna(&backend).is_empty());
    }

    #[test]
    fn le_magasin_apprend_les_cles_de_reconnaissance() {
        // Un magasin ancien ne doit pas rester eternellement au niveau le plus
        // faible : apres un re-sondage reussi il porte MAC, marque et modele,
        // et le desaccord SUIVANT se tranche sur la MAC.
        let backend = memory_backend();
        entree_marantz(&backend);
        persist_discovered_dlna(
            &backend,
            &DiscoveredDlnaDevice::new(
                "uuid:56fcb4ae-e909-1c8d-0080-0006787c2e26",
                AIOS_LOCATION,
                "Marantz ND8006",
                "192.168.1.11",
                60006,
            )
            .with_identity("00:06:78:7C:2E:26", "Marantz", "ND8006"),
        );

        let stored = load_discovered_dlna(&backend);
        assert_eq!(stored.len(), 1, "l'entree est mise a jour, pas dupliquee");
        assert_eq!(stored[0].mac, "00:06:78:7C:2E:26");

        // Meme nom, meme modele, MAC differente : c'est un autre exemplaire.
        let jumeau = RendererIdentity {
            udn: "uuid:c0bfdbad-45f0-dfe0-819a-c4bcec2cce65",
            mac: "00:06:78:AA:BB:CC",
            friendly_name: "Marantz ND8006",
            manufacturer: "Marantz",
            model_name: "ND8006",
        };
        assert_eq!(
            compare_at_same_location(stored[0].identity(), jumeau),
            IdentityVerdict::OtherHardware(Evidence::Mac)
        );
    }

    #[test]
    fn un_magasin_d_avant_2639_se_relit_sans_perdre_ses_entrees() {
        // Le risque a ne pas prendre : trois champs ajoutes a une structure
        // deserialisee depuis un JSON deja en base. Sans `#[serde(default)]`,
        // `from_str` echoue, `load_discovered_dlna` rend un magasin VIDE, et
        // toutes les zones DLNA disparaissent d'un coup au premier demarrage.
        let ancien = r#"[{"uuid":"uuid:56fcb4ae","location":"http://192.168.1.11:60006/d.xml","name":"Marantz ND8006","host":"192.168.1.11","port":60006}]"#;
        let relu: Vec<DiscoveredDlnaDevice> = serde_json::from_str(ancien)
            .expect("un magasin ecrit avant #2639 doit continuer de se relire");
        assert_eq!(relu.len(), 1);
        assert_eq!(relu[0].name, "Marantz ND8006");
        assert!(relu[0].mac.is_empty());
        assert!(relu[0].manufacturer.is_empty());
        assert!(relu[0].model.is_empty());
    }

    // ── Un appareil physique = une LOCATION (#1703) ───────────────────────
    //
    // Journaux de Jean Valjean (0.9.71) : 86 lignes pour `host=192.168.1.11`,
    // CINQ uuid distincts, tous a la meme URL de description
    // `http://192.168.1.11:60006/upnp/desc/aios_device/aios_device.xml`.
    // C'est un HEOS Denon/Marantz : il annonce sa racine AiOS et chacun de
    // ses appareils embarques (MediaRenderer, MediaServer, ACT-Denon…) sous
    // un `uuid:` different mais derriere une seule description racine.

    const AIOS_LOCATION: &str = "http://192.168.1.11:60006/upnp/desc/aios_device/aios_device.xml";

    fn memory_backend() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // La table `settings` naît d'une migration : sans elle le magasin
        // relit toujours vide et le test ne prouve rien.
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn les_udn_freres_d_un_heos_ne_font_qu_une_entree() {
        let backend = memory_backend();
        // Les cinq UDN du ND8006, dans l'ordre ou SSDP les annonce. Trois
        // partagent le suffixe `-0080-0006787c2e26` : meme materiel.
        for uuid in [
            "uuid:9ab0c000-f668-11de-9976-0080-0006787c2e26",
            "uuid:9ab0c001-f668-11de-9976-0080-0006787c2e26",
            "uuid:9ab0c002-f668-11de-9976-0080-0006787c2e26",
            "uuid:5f9ec1b3-ff59-19bb-8530-0006787c2e26",
            "uuid:a2c8e5d1-0011-2233-4455-0006787c2e26",
        ] {
            persist_discovered_dlna(
                &backend,
                &DiscoveredDlnaDevice::new(
                    uuid,
                    AIOS_LOCATION,
                    "Marantz ND8006",
                    "192.168.1.11",
                    60006,
                ),
            );
        }

        // Sans le correctif : cinq entrees, donc 5 x 8 = 80 sondages au
        // demarrage suivant. Avec : une seule.
        let stored = load_discovered_dlna(&backend);
        assert_eq!(
            stored.len(),
            1,
            "un seul appareil physique doit donner une seule entree, pas {} : {:?}",
            stored.len(),
            stored.iter().map(|d| &d.uuid).collect::<Vec<_>>()
        );
        assert_eq!(stored[0].location, AIOS_LOCATION);
    }

    #[test]
    fn deux_lecteurs_distincts_gardent_chacun_leur_entree() {
        // Le garde-fou a ne pas casser : deux LOCATION differentes sont deux
        // appareils, meme derriere la meme adresse (ampli multi-zone, hote
        // faisant tourner deux renderers).
        let backend = memory_backend();
        persist_discovered_dlna(
            &backend,
            &DiscoveredDlnaDevice::new(
                "uuid:zone-1",
                "http://192.168.1.11:8080/desc.xml",
                "Ampli Zone 1",
                "192.168.1.11",
                8080,
            ),
        );
        persist_discovered_dlna(
            &backend,
            &DiscoveredDlnaDevice::new(
                "uuid:zone-2",
                "http://192.168.1.11:8081/desc.xml",
                "Ampli Zone 2",
                "192.168.1.11",
                8081,
            ),
        );
        assert_eq!(load_discovered_dlna(&backend).len(), 2);
    }

    #[test]
    fn un_magasin_deja_dedouble_se_replie_au_demarrage() {
        // Les installations qui tournent deja ont les cinq entrees en base :
        // le repli doit les guerir sans attendre le rejet definitif de #1647.
        let stored: Vec<DiscoveredDlnaDevice> = (0..5)
            .map(|i| {
                DiscoveredDlnaDevice::new(
                    &format!("uuid:aios-{i}"),
                    AIOS_LOCATION,
                    "Marantz ND8006",
                    "192.168.1.11",
                    60006,
                )
            })
            .collect();
        let collapsed = dedup_dlna_by_location(stored);
        assert_eq!(collapsed.len(), 1);
        // On garde la premiere entree, celle qui a ete decouverte en premier.
        assert_eq!(collapsed[0].uuid, "uuid:aios-0");
    }
}

#[cfg(test)]
mod list_devices_dedup_tests {
    use super::*;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::discovery::device::{DiscoveredDevice, OutputType};

    fn zone_repo() -> ZoneRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        ZoneRepo::new(db)
    }

    fn marantz_deux_identites() -> Vec<DiscoveredDevice> {
        // Cas documenté dans #1880 : le même appareil s'annonce en mDNS
        // (identité AirPlay) puis en SSDP (identité UPnP), même hôte.
        vec![
            DiscoveredDevice::new(
                "airplay-00:06:78:7C:2E:26".into(),
                "Marantz ND8006".into(),
                OutputType::Airplay,
                "192.168.1.50".into(),
                7000,
            ),
            DiscoveredDevice::new(
                "uuid:56fcb4ae-8f52-4a80-9d1c-000000000000".into(),
                "Marantz ND8006".into(),
                OutputType::Openhome,
                "192.168.1.50".into(),
                1400,
            ),
        ]
    }

    #[test]
    fn liste_replie_les_identites_multiples_d_un_meme_hote() {
        // GET /devices doit replier les identités d'un même hôte comme
        // POST /devices/scan le fait déjà (issue #2452).
        let items = build_device_list(
            marantz_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        assert_eq!(
            items.len(),
            1,
            "un appareil à deux identités doit produire UNE entrée, obtenu : {items:?}"
        );
        // La priorité Openhome > Airplay choisit l'identité UPnP en primaire.
        assert_eq!(
            items[0].get("id").and_then(|v| v.as_str()),
            Some("uuid:56fcb4ae-8f52-4a80-9d1c-000000000000")
        );
        // L'identité secondaire reste accessible dans capabilities.alternatives.
        let alts = items[0]
            .get("capabilities")
            .and_then(|c| c.get("alternatives"))
            .and_then(|a| a.as_array())
            .expect("capabilities.alternatives doit exister");
        assert_eq!(
            alts[0].get("id").and_then(|v| v.as_str()),
            Some("airplay-00:06:78:7C:2E:26")
        );
    }

    #[test]
    fn backfill_ne_ressuscite_pas_une_identite_secondaire_enregistree() {
        // Si l'identité secondaire est enregistrée comme sortie, la boucle de
        // rattrapage (outputs enregistrés absents de la découverte) ne doit
        // pas la réintroduire comme une deuxième entrée.
        let mut registered = std::collections::HashSet::new();
        registered.insert("airplay-00:06:78:7C:2E:26".to_string());
        let output_info = vec![json!({
            "device_id": "airplay-00:06:78:7C:2E:26",
            "name": "Marantz ND8006",
            "type": "airplay",
            "host": "192.168.1.50",
        })];
        let items = build_device_list(marantz_deux_identites(), &registered, &output_info);
        assert_eq!(
            items.len(),
            1,
            "l'identité secondaire enregistrée ne doit pas réapparaître, obtenu : {items:?}"
        );
        // L'appareil est bien marqué enregistré, via son identité secondaire.
        assert_eq!(
            items[0].get("registered").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn format_inchange_pour_des_appareils_distincts() {
        // Deux hôtes distincts : aucune fusion, et le format de réponse
        // (available / registered / type) est inchangé.
        let devices = vec![
            DiscoveredDevice::new(
                "dlna-a".into(),
                "Salon".into(),
                OutputType::Dlna,
                "192.168.1.10".into(),
                1400,
            ),
            DiscoveredDevice::new(
                "dlna-b".into(),
                "Cuisine".into(),
                OutputType::Dlna,
                "192.168.1.11".into(),
                1400,
            ),
        ];
        let items = build_device_list(devices, &std::collections::HashSet::new(), &[]);
        assert_eq!(items.len(), 2);
        for it in &items {
            assert_eq!(it.get("available").and_then(|v| v.as_bool()), Some(true));
            assert_eq!(it.get("registered").and_then(|v| v.as_bool()), Some(false));
            assert!(it.get("type").and_then(|v| v.as_str()).is_some());
        }
    }

    #[test]
    fn backfill_conserve_les_sorties_enregistrees_hors_decouverte() {
        // Une sortie enregistrée absente de la découverte doit toujours
        // apparaître (comportement historique, à ne pas régresser).
        let output_info = vec![json!({
            "device_id": "dlna-orphelin",
            "name": "Chambre",
            "type": "dlna",
            "host": "192.168.1.77",
        })];
        let items = build_device_list(Vec::new(), &std::collections::HashSet::new(), &output_info);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("id").and_then(|v| v.as_str()),
            Some("dlna-orphelin")
        );
        assert_eq!(
            items[0].get("registered").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn appareil_decouvert_signale_la_zone_masquee_et_son_identite_exacte() {
        let repo = zone_repo();
        let hidden_id = "airplay-00:06:78:7C:2E:26";
        let zone_id = repo
            .create("Marantz ND8006", Some("airplay"), Some(hidden_id))
            .unwrap();
        repo.delete(zone_id).unwrap();

        let mut items = build_device_list(
            marantz_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        mark_hidden_zones(&mut items, &repo);

        assert_eq!(
            items[0].get("zone_hidden").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            items[0]
                .get("hidden_zone_device_id")
                .and_then(Value::as_str),
            Some(hidden_id),
            "la restauration doit viser l'identité masquée, même secondaire"
        );
    }

    #[test]
    fn appareil_sans_zone_masquee_ne_propose_pas_de_restauration() {
        let repo = zone_repo();
        let mut items = build_device_list(
            marantz_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        mark_hidden_zones(&mut items, &repo);

        assert_eq!(
            items[0].get("zone_hidden").and_then(Value::as_bool),
            Some(false)
        );
        assert!(items[0].get("hidden_zone_device_id").is_none());
    }
}

/// #1280 — la liste `GET /devices` ne propose plus les appareils ignorés.
///
/// Le ticket est né de CETTE liste : « ils disparaissent bien sur le coup mais
/// réapparaissent rapidement » (Patatorz). Empêcher la création de zone ne
/// suffisait donc pas — c'est la proposition qu'il faut faire taire.
///
/// Le couple [`un_appareil_ignore_sort_de_la_liste`] /
/// [`la_meme_liste_sans_blocage_garde_l_appareil`] est la contre-épreuve
/// PERMANENTE : même liste, même appareil, seule la ligne d'ignorance change.
/// Un filtre bloqué sur « tout retirer » rend le second rouge, bloqué sur
/// « ne rien retirer » il rend le premier rouge.
#[cfg(test)]
mod liste_appareils_ignores_1280 {
    use super::*;
    use tune_core::db::ignored_device_repo::{IgnoredDevice, IgnoredDeviceRepo};
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::discovery::device::{DiscoveredDevice, OutputType};

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        Arc::new(db)
    }

    fn faire_taire(db: &Arc<dyn DbBackend>, device_id: &str, host: &str, name: &str) {
        IgnoredDeviceRepo::with_backend(db.clone())
            .ignore(&IgnoredDevice {
                device_id: device_id.into(),
                mac: String::new(),
                host: host.into(),
                name: name.into(),
                device_type: "dlna".into(),
                created_at: None,
            })
            .unwrap();
    }

    /// Un Sonos vu sous DEUX identités au même hôte : `build_device_list` les
    /// replie en une entrée, l'identité secondaire allant dans
    /// `capabilities.alternatives`.
    fn sonos_deux_identites() -> Vec<DiscoveredDevice> {
        vec![
            DiscoveredDevice::new(
                "uuid:sonos-dlna".into(),
                "Chambre - Sonos One".into(),
                OutputType::Dlna,
                "192.168.1.50".into(),
                1400,
            ),
            DiscoveredDevice::new(
                "uuid:sonos-openhome".into(),
                "Chambre - Sonos One".into(),
                OutputType::Openhome,
                "192.168.1.50".into(),
                1400,
            ),
        ]
    }

    /// `Router` panique à la CONSTRUCTION sur un conflit de routes. Ce test
    /// fixe donc que `/ignored` (statique) et `/{device_id}/ignore`
    /// (dynamique) cohabitent — le serveur ne démarrerait pas sinon.
    #[test]
    fn les_routes_d_ignorance_ne_percutent_aucune_route_existante() {
        let _ = router();
    }

    #[test]
    fn un_appareil_ignore_sort_de_la_liste() {
        let db = base();
        faire_taire(
            &db,
            "uuid:sonos-dlna",
            "192.168.1.50",
            "Chambre - Sonos One",
        );

        let mut items = build_device_list(
            sonos_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        retirer_appareils_ignores(&mut items, &db);

        assert!(
            items.is_empty(),
            "l'appareil ignoré ne doit plus être proposé, obtenu : {items:?}"
        );
    }

    /// CONTRE-ÉPREUVE PERMANENTE : la même liste, sans blocage, garde son
    /// appareil.
    #[test]
    fn la_meme_liste_sans_blocage_garde_l_appareil() {
        let db = base();
        let mut items = build_device_list(
            sonos_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        retirer_appareils_ignores(&mut items, &db);
        assert_eq!(items.len(), 1, "sans blocage, l'appareil reste proposé");
    }

    /// L'utilisateur a pu faire taire l'appareil depuis son identité
    /// SECONDAIRE (celle repliée dans `alternatives`). L'entrée, portée par
    /// l'identité primaire, doit disparaître quand même.
    #[test]
    fn ignorer_une_identite_secondaire_retire_l_entree_entiere() {
        let db = base();
        let items = build_device_list(
            sonos_deux_identites(),
            &std::collections::HashSet::new(),
            &[],
        );
        let primaire = items[0]
            .get("id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let secondaire = items[0]
            .get("capabilities")
            .and_then(|c| c.get("alternatives"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_ne!(primaire, secondaire);

        // On fige UNIQUEMENT l'identité secondaire, sans hôte ni nom : seule
        // la règle d'identité EXACTE peut donc jouer.
        IgnoredDeviceRepo::with_backend(db.clone())
            .ignore(&IgnoredDevice {
                device_id: secondaire,
                mac: String::new(),
                host: String::new(),
                name: String::new(),
                device_type: "openhome".into(),
                created_at: None,
            })
            .unwrap();

        let mut items = items;
        retirer_appareils_ignores(&mut items, &db);
        assert!(
            items.is_empty(),
            "faire taire une identité secondaire doit retirer l'appareil, \
             obtenu : {items:?}"
        );
    }

    /// GARDE-FOU ANTI-DHCP (#1651) : un autre appareil ayant hérité de
    /// l'adresse reste listé.
    #[test]
    fn un_autre_appareil_au_meme_hote_reste_liste() {
        let db = base();
        faire_taire(
            &db,
            "uuid:sonos-dlna",
            "192.168.1.50",
            "Chambre - Sonos One",
        );

        let mut items = build_device_list(
            vec![DiscoveredDevice::new(
                "uuid:cabasse".into(),
                "Cabasse Pearl Akoya".into(),
                OutputType::Dlna,
                "192.168.1.50".into(),
                1400,
            )],
            &std::collections::HashSet::new(),
            &[],
        );
        retirer_appareils_ignores(&mut items, &db);

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("name").and_then(Value::as_str),
            Some("Cabasse Pearl Akoya")
        );
    }
}
