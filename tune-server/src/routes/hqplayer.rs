use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(hqp_status))
        .route("/config", get(hqp_config).post(set_hqp_config))
        .route("/play", post(hqp_play))
        .route("/filters", get(hqp_filters))
        .route("/outputs", get(hqp_outputs))
}

fn hqp_settings(state: &AppState) -> (String, u16) {
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
        .unwrap_or(4321);
    (host, port)
}

/// Check HQPlayer connectivity by attempting a TCP connection.
async fn hqp_status(State(state): State<AppState>) -> Json<Value> {
    let (host, port) = hqp_settings(&state);

    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    Json(json!({
        "configured": true,
        "connected": connected,
        "host": host,
        "port": port,
        "protocol": "tcp",
        "message": if connected {
            "HQPlayer is reachable"
        } else {
            "HQPlayer is not reachable — check host/port"
        },
    }))
}

/// Get HQPlayer configuration.
async fn hqp_config(State(state): State<AppState>) -> Json<Value> {
    let (host, port) = hqp_settings(&state);
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
        "hqplayer_auto_detect": auto_detect,
    }))
}

#[derive(Deserialize)]
struct HqpConfigBody {
    hqplayer_host: Option<String>,
    hqplayer_port: Option<u16>,
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
    if let Some(auto) = body.hqplayer_auto_detect {
        settings
            .set("hqplayer_auto_detect", if auto { "true" } else { "false" })
            .ok();
    }
    Json(json!({"saved": true}))
}

#[derive(Deserialize)]
struct HqpPlayRequest {
    /// URI of the track to play
    uri: String,
    /// Desired output filter (optional)
    filter: Option<String>,
}

/// Send a play command to HQPlayer (stub — real implementation requires HQPlayer TCP protocol).
async fn hqp_play(
    State(state): State<AppState>,
    Json(body): Json<HqpPlayRequest>,
) -> impl IntoResponse {
    let (host, port) = hqp_settings(&state);

    // HQPlayer uses a proprietary XML-over-TCP protocol.
    // Real implementation would open a TCP socket and send XML commands.
    // For now, verify connectivity and return a stub.
    let connected = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        tokio::net::TcpStream::connect(format!("{host}:{port}")),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);

    if !connected {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "HQPlayer not reachable",
                "host": host,
                "port": port,
            })),
        )
            .into_response();
    }

    Json(json!({
        "status": "play_command_stub",
        "uri": body.uri,
        "filter": body.filter,
        "message": "HQPlayer play command requires TCP protocol implementation. Connection verified.",
    }))
    .into_response()
}

/// List available HQPlayer filters (stub).
async fn hqp_filters(State(state): State<AppState>) -> Json<Value> {
    let (host, port) = hqp_settings(&state);
    // Real implementation would query HQPlayer for available filters via TCP.
    Json(json!({
        "host": host,
        "port": port,
        "filters": [
            {"id": "poly-sinc-short-mp", "name": "poly-sinc-short-mp", "type": "minimum-phase"},
            {"id": "poly-sinc-long-lp", "name": "poly-sinc-long-lp", "type": "linear-phase"},
            {"id": "poly-sinc-gauss-long-hires-lp", "name": "poly-sinc-gauss-long-hires-lp", "type": "linear-phase"},
            {"id": "poly-sinc-gauss-xla-hires-mp", "name": "poly-sinc-gauss-xla-hires-mp", "type": "minimum-phase"},
        ],
        "message": "Stub filter list — real list requires HQPlayer TCP query",
    }))
}

/// List available HQPlayer outputs (stub).
async fn hqp_outputs(State(state): State<AppState>) -> Json<Value> {
    let (host, port) = hqp_settings(&state);
    Json(json!({
        "host": host,
        "port": port,
        "outputs": [],
        "message": "Output list requires HQPlayer TCP protocol query",
    }))
}
