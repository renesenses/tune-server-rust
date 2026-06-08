use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::hqplayer::HQPLAYER_DEFAULT_PORT;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(hqp_status))
        .route("/config", get(hqp_config).post(set_hqp_config))
        .route("/discover", post(hqp_discover))
        .route("/play", post(hqp_play))
        .route("/filters", get(hqp_filters))
        .route("/outputs", get(hqp_outputs))
}

fn hqp_settings(state: &AppState) -> (String, u16, bool) {
    let settings = SettingsRepo::new(state.db.clone());
    let host = settings
        .get("hqplayer_host")
        .ok()
        .flatten()
        .unwrap_or_else(|| "localhost".into());
    let port = settings
        .get("hqplayer_port")
        .ok()
        .flatten()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(HQPLAYER_DEFAULT_PORT);
    let enabled = settings
        .get("hqplayer_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    (host, port, enabled)
}

/// Check HQPlayer connectivity via its HTTP API.
async fn hqp_status(State(state): State<AppState>) -> Json<Value> {
    let (host, port, enabled) = hqp_settings(&state);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let url = format!("http://{host}:{port}/api/status");
    let connected = client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    Json(json!({
        "configured": enabled,
        "connected": connected,
        "host": host,
        "port": port,
        "protocol": "http",
        "message": if connected {
            "HQPlayer is reachable"
        } else {
            "HQPlayer is not reachable \u{2014} check host/port"
        },
    }))
}

/// Get HQPlayer configuration.
async fn hqp_config(State(state): State<AppState>) -> Json<Value> {
    let (host, port, enabled) = hqp_settings(&state);
    let settings = SettingsRepo::new(state.db.clone());
    let auto_detect = settings
        .get("hqplayer_auto_detect")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "hqplayer_host": host,
        "hqplayer_port": port,
        "hqplayer_enabled": enabled,
        "hqplayer_auto_detect": auto_detect,
    }))
}

#[derive(Deserialize)]
struct HqpConfigBody {
    hqplayer_host: Option<String>,
    hqplayer_port: Option<u16>,
    hqplayer_enabled: Option<bool>,
    hqplayer_auto_detect: Option<bool>,
}

async fn set_hqp_config(
    State(state): State<AppState>,
    Json(body): Json<HqpConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db.clone());
    if let Some(host) = &body.hqplayer_host {
        settings.set("hqplayer_host", host).ok();
    }
    if let Some(port) = body.hqplayer_port {
        settings.set("hqplayer_port", &port.to_string()).ok();
    }
    if let Some(enabled) = body.hqplayer_enabled {
        settings
            .set("hqplayer_enabled", if enabled { "true" } else { "false" })
            .ok();
    }
    if let Some(auto) = body.hqplayer_auto_detect {
        settings
            .set("hqplayer_auto_detect", if auto { "true" } else { "false" })
            .ok();
    }

    // If enabled after config change, trigger discovery
    let (host, port, enabled) = hqp_settings(&state);
    if enabled {
        let _ = discover_and_register_inner(&state, &host, port).await;
    }

    Json(json!({"saved": true}))
}

/// Manually trigger HQPlayer discovery.
async fn hqp_discover(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port, _) = hqp_settings(&state);
    match discover_and_register_inner(&state, &host, port).await {
        Ok(info) => Json(info).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Register an HQPlayer output and auto-create a zone if HQPlayer is reachable.
/// Called at startup poll and via POST /hqplayer/discover.
pub async fn discover_and_register(state: &AppState) -> Result<Value, String> {
    let (host, port, _) = hqp_settings(state);
    discover_and_register_inner(state, &host, port).await
}

async fn discover_and_register_inner(
    state: &AppState,
    host: &str,
    port: u16,
) -> Result<Value, String> {
    // Check if HQPlayer is reachable
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();
    let url = format!("http://{host}:{port}/api/status");
    let reachable = client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    if !reachable {
        return Err(format!("HQPlayer not reachable at {host}:{port}"));
    }

    let device_id = format!("hqplayer-{host}");
    let output_name = "HQPlayer".to_string();

    // Register output
    let output = tune_core::outputs::hqplayer::HqplayerOutput::new(
        output_name.clone(),
        device_id.clone(),
        host.to_string(),
        port,
    );
    {
        let mut reg = state.outputs.lock().await;
        reg.register(Box::new(output));
    }
    tracing::info!(name = %output_name, id = %device_id, host = %host, port, "hqplayer_output_registered");

    // Auto-create zone if not already present
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let existing = zone_repo.list().unwrap_or_default();
    let already = existing
        .iter()
        .any(|z| z.output_device_id.as_deref() == Some(&*device_id));
    if already {
        let _ = zone_repo.set_online_by_device(&device_id, true);
        tracing::info!(name = %output_name, id = %device_id, "hqplayer_zone_reconnected");
    } else {
        let name_taken = existing.iter().any(|z| z.name == output_name);
        let zone_name = if name_taken {
            format!("HQPlayer ({host})")
        } else {
            output_name.clone()
        };
        if let Ok(zid) = zone_repo.create(&zone_name, Some("hqplayer"), Some(&device_id)) {
            tracing::info!(name = %zone_name, zone_id = zid, "hqplayer_zone_auto_created");
        }
    }

    Ok(json!({
        "discovered": true,
        "device_id": device_id,
        "name": output_name,
        "host": host,
        "port": port,
    }))
}

#[derive(Deserialize)]
struct HqpPlayRequest {
    /// URI of the track to play
    uri: String,
    /// Desired output filter (optional)
    filter: Option<String>,
}

/// Send a play command to HQPlayer via its HTTP API.
async fn hqp_play(
    State(state): State<AppState>,
    Json(body): Json<HqpPlayRequest>,
) -> impl IntoResponse {
    let (host, port, _) = hqp_settings(&state);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let base = format!("http://{host}:{port}");

    // Set source URI
    let encoded = urlencoding::encode(&body.uri);
    let set_resp = client
        .post(format!("{base}/api/source/uri?uri={encoded}"))
        .send()
        .await;
    if let Err(e) = &set_resp {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("Failed to set source: {e}"),
                "host": host,
                "port": port,
            })),
        )
            .into_response();
    }

    // Play
    let play_resp = client
        .post(format!("{base}/api/transport/play"))
        .send()
        .await;
    if let Err(e) = &play_resp {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("Failed to play: {e}"),
                "host": host,
                "port": port,
            })),
        )
            .into_response();
    }

    Json(json!({
        "status": "playing",
        "uri": body.uri,
        "filter": body.filter,
        "host": host,
        "port": port,
    }))
    .into_response()
}

/// List available HQPlayer filters (stub until HQPlayer exposes them via HTTP).
async fn hqp_filters(State(state): State<AppState>) -> Json<Value> {
    let (host, port, _) = hqp_settings(&state);
    Json(json!({
        "host": host,
        "port": port,
        "filters": [],
        "message": "Filter list requires HQPlayer to expose /api/filters endpoint",
    }))
}

/// List available HQPlayer outputs (stub until HQPlayer exposes them via HTTP).
async fn hqp_outputs(State(state): State<AppState>) -> Json<Value> {
    let (host, port, _) = hqp_settings(&state);
    Json(json!({
        "host": host,
        "port": port,
        "outputs": [],
        "message": "Output list requires HQPlayer to expose /api/outputs endpoint",
    }))
}
