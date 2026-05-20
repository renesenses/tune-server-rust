use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use tune_core::discovery::device::dedup_devices;
use tune_core::discovery::ssdp::SsdpScanner;
use tune_core::discovery::xml_parser::fetch_device_description;
use tune_core::outputs::dlna::DlnaOutput;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/list", get(list_devices))
        .route("/scan", post(scan_devices))
        .route("/audio", get(list_audio_devices))
        .route("/{device_id}/status", get(device_status))
}

async fn list_devices(State(state): State<AppState>) -> Json<Value> {
    let scanner = state.scanner.lock().await;
    let discovered = scanner.devices().await;
    drop(scanner);

    let outputs = state.outputs.lock().await;
    let registered_ids: std::collections::HashSet<String> = outputs.list().into_iter().collect();

    let items: Vec<Value> = discovered
        .iter()
        .map(|d| {
            let mut v = serde_json::to_value(d).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), json!(true));
                obj.insert("registered".into(), json!(registered_ids.contains(&d.id)));
                obj.insert("type".into(), json!(d.device_type.to_string()));
            }
            v
        })
        .collect();

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
            if location.is_empty() { continue; }

            if let Ok(desc) = fetch_device_description(location).await {
                if desc.is_media_renderer() {
                    let service_urls = desc.service_urls();
                    let av_url = service_urls.get("urn:schemas-upnp-org:service:AVTransport:1");
                    let rc_url = service_urls.get("urn:schemas-upnp-org:service:RenderingControl:1");

                    if let (Some(av), Some(rc)) = (av_url, rc_url) {
                        let base = format!("http://{}:{}", d.host, d.port);
                        let dlna = DlnaOutput::new(
                            d.name.clone(),
                            d.id.clone(),
                            d.host.clone(),
                            format!("{base}{av}"),
                            format!("{base}{rc}"),
                        );
                        outputs.register(Box::new(dlna));
                        registered += 1;
                    }
                }
            }
        }
    }

    let items: Vec<Value> = deduped
        .iter()
        .map(|d| json!({
            "id": d.id,
            "name": d.name,
            "type": format!("{:?}", d.device_type),
            "host": d.host,
            "port": d.port,
            "available": d.available,
            "manufacturer": d.manufacturer,
            "model": d.model,
        }))
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
        return Json(json!(devices));
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
