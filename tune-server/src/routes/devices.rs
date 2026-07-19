use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::dedup_devices;
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::outputs::bluos::BluosOutput;
use tune_core::outputs::dlna::DlnaOutput;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/list", get(list_devices))
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
        .route("/{device_id}", axum::routing::delete(delete_device))
        .route("/{device_id}/pair", post(pair_device))
        .route("/{device_id}/pair/pin", post(pair_device_pin))
        .route(
            "/{device_id}/airplay2/pair-pin-start",
            post(airplay2_pair_pin_start),
        )
}

async fn list_devices(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let discovered = scanner.devices().await;
    drop(scanner);

    let outputs = state.outputs.lock().await;
    let registered_ids: std::collections::HashSet<String> = outputs.list().into_iter().collect();
    // Use info_all() instead of status_all() to avoid sequential is_available() probes
    // that can block for seconds per unreachable DLNA device, causing the entire
    // endpoint to time out and return 0 DLNA devices.
    let all_output_info = outputs.info_all().await;
    drop(outputs);

    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut items: Vec<Value> = discovered
        .iter()
        .map(|d| {
            seen_ids.insert(d.id.clone());
            let mut v = serde_json::to_value(d).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), json!(true));
                obj.insert("registered".into(), json!(registered_ids.contains(&d.id)));
                obj.insert("type".into(), json!(d.device_type.to_string()));
            }
            v
        })
        .collect();

    // Add any registered outputs not already present from SSDP discovery.
    // This ensures DLNA/OpenHome devices appear even when the SSDP scanner's
    // internal device list is empty (e.g., between scan cycles or after restart).
    for output_info in &all_output_info {
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
            }));
        }
    }

    Json(json!(items))
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
            let delay = state.config.play_delay_for(&device_name);
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
    let scanner = state.scanner.lock().await;
    let devices = scanner.rescan().await;
    drop(scanner);

    let deduped = dedup_devices(devices);

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
                    let delay = state.config.play_delay_for(&d.name);
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
        let backend = &state.config.local_audio_backend;
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
        Json(json!({
            "devices": devices,
            "backend": tune_core::outputs::local::active_backend_name(backend),
            "asio_available": tune_core::outputs::local::asio_available(),
        }))
    }
    #[cfg(not(feature = "local-audio"))]
    {
        let _ = state;
        Json(json!({
            "devices": [],
            "backend": "none",
            "asio_available": false,
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
