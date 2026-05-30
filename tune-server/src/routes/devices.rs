use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::discovery::device::dedup_devices;
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::outputs::dlna::DlnaOutput;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/list", get(list_devices))
        .route("/scan", post(scan_devices))
        .route("/audio", get(list_audio_devices))
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
}

async fn list_devices(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let discovered = scanner.devices().await;
    drop(scanner);

    let outputs = state.outputs.lock().await;
    let registered_ids: std::collections::HashSet<String> = outputs.list().into_iter().collect();
    let all_output_status = outputs.status_all().await;
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

    for output_info in &all_output_status {
        if let Some(device_id) = output_info.get("device_id").and_then(|v| v.as_str()) {
            if seen_ids.contains(device_id) {
                continue;
            }
            let name = output_info
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let output_type = output_info
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let available = output_info
                .get("available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
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
                "available": available,
                "registered": true,
            }));
        }
    }

    Json(json!(items))
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
                    let dlna = DlnaOutput::new(
                        d.name.clone(),
                        d.id.clone(),
                        d.host.clone(),
                        format!("{base}{av}"),
                        format!("{base}{rc}"),
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

async fn list_audio_devices() -> Json<Value> {
    #[cfg(feature = "local-audio")]
    {
        let devices = tune_core::outputs::local::list_audio_devices();
        Json(json!(devices))
    }
    #[cfg(not(feature = "local-audio"))]
    Json(json!([]))
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

fn buffer_settings_for(db: &tune_core::db::sqlite::SqliteDb, device_id: &str) -> (f64, bool) {
    let settings = SettingsRepo::new(db.clone());
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
            let (buffer_s, auto) = buffer_settings_for(&state.db, device_id);
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
    let (buffer_s, auto) = buffer_settings_for(&state.db, &device_id);
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

    let (current_buf, current_auto) = buffer_settings_for(&state.db, &device_id);
    let new_buf = body.buffer_s.unwrap_or(current_buf);
    let new_auto = body.auto.unwrap_or(current_auto);

    let settings = SettingsRepo::new(state.db);
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
    Json(json!({"cleared": removed}))
}

async fn delete_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> impl IntoResponse {
    let mut outputs = state.outputs.lock().await;
    outputs.remove(&device_id);
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
    let settings = SettingsRepo::new(state.db);
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
    let settings = SettingsRepo::new(state.db);
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
