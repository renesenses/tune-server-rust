use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use tune_core::discovery::device::dedup_devices;
use tune_core::discovery::ssdp::SsdpScanner;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_devices))
        .route("/scan", post(scan_devices))
        .route("/audio", get(list_audio_devices))
}

async fn list_devices(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "use POST /scan to discover devices",
    }))
}

async fn scan_devices(State(_state): State<AppState>) -> Json<Value> {
    let (ssdp_tx, _ssdp_rx) = tokio::sync::mpsc::channel(64);
    let mut ssdp = SsdpScanner::new(ssdp_tx);
    ssdp.start().await;

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let devices = ssdp.devices().await;
    ssdp.stop().await;

    let deduped = dedup_devices(devices);

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
    }))
}

async fn list_audio_devices() -> Json<Value> {
    Json(json!({
        "items": [],
        "message": "local audio device enumeration requires cpal (Phase 7)",
    }))
}
