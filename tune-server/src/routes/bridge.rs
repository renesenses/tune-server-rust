use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use tune_core::outputs::bridge::{BridgeCommand, BridgeOutput, BridgeResponse};

use crate::state::AppState;

#[derive(serde::Deserialize)]
struct BridgeQuery {
    api_key: Option<String>,
}

#[derive(serde::Deserialize)]
struct BridgeHello {
    bridge_id: String,
    bridge_name: String,
    #[allow(dead_code)]
    version: Option<String>,
}

#[derive(serde::Deserialize)]
struct BridgeDevice {
    id: String,
    name: String,
    device_type: String,
    #[allow(dead_code)]
    host: String,
    #[allow(dead_code)]
    port: u16,
    #[allow(dead_code)]
    manufacturer: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(serde::Deserialize)]
struct BridgeDevices {
    devices: Vec<BridgeDevice>,
}

#[derive(serde::Deserialize)]
struct BridgeDeviceLost {
    device_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(ws_handler))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<BridgeQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Authenticate via API key
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let stored_key = settings.get("api_key").ok().flatten().unwrap_or_default();

    let provided_key = query.api_key.unwrap_or_default();
    if stored_key.is_empty() || provided_key != stored_key {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| handle_bridge(socket, state))
        .into_response()
}

async fn handle_bridge(mut socket: WebSocket, state: AppState) {
    // Wait for Hello message
    let hello: BridgeHello = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                let msg: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if msg.get("type").and_then(|t| t.as_str()) == Some("bridge.hello") {
                    match serde_json::from_value::<BridgeHello>(msg) {
                        Ok(h) => break h,
                        Err(_) => continue,
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            None | Some(Err(_)) => return,
            _ => continue,
        }
    };

    info!(
        bridge_id = %hello.bridge_id,
        bridge_name = %hello.bridge_name,
        "bridge connected"
    );

    let bridge_id = hello.bridge_id.clone();
    let connected = Arc::new(AtomicBool::new(true));

    // Channel for sending commands to the bridge
    let (command_tx, mut command_rx) = mpsc::channel::<BridgeCommand>(64);

    // Track all devices registered by this bridge
    let registered_devices: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let registered_for_cleanup = registered_devices.clone();

    use futures_util::StreamExt as _;
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Writer task: forwards commands to WebSocket
    let writer_connected = connected.clone();
    let writer_handle = tokio::spawn(async move {
        use futures_util::SinkExt;
        while let Some(cmd) = command_rx.recv().await {
            let msg = serde_json::json!({
                "type": "bridge.command",
                "id": cmd.id,
                "device_id": cmd.device_id,
                "command": cmd.command,
                "payload": cmd.payload,
            });
            if ws_tx
                .send(axum::extract::ws::Message::Text(msg.to_string().into()))
                .await
                .is_err()
            {
                writer_connected.store(false, Ordering::Relaxed);
                break;
            }
        }
    });

    // Reader loop: process bridge messages
    loop {
        match ws_rx.next().await {
            Some(Ok(axum::extract::ws::Message::Text(text))) => {
                let msg: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let msg_type = msg
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();

                match msg_type.as_str() {
                    "bridge.devices" => {
                        if let Ok(devs) = serde_json::from_value::<BridgeDevices>(msg) {
                            handle_devices(
                                &state,
                                &bridge_id,
                                &devs.devices,
                                &command_tx,
                                &connected,
                                &registered_devices,
                            )
                            .await;
                        }
                    }
                    "bridge.device_lost" => {
                        if let Ok(lost) = serde_json::from_value::<BridgeDeviceLost>(msg) {
                            let full_id = format!("bridge:{}:{}", bridge_id, lost.device_id);
                            let mut reg = state.outputs.lock().await;
                            reg.remove(&full_id);
                            registered_devices.lock().await.retain(|d| d != &full_id);

                            let zone_repo =
                                tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
                            let _ = zone_repo.set_online_by_device(&full_id, false);

                            info!(device_id = %full_id, "bridge device lost");
                        }
                    }
                    "bridge.response" => {
                        if let Ok(resp) = serde_json::from_value::<BridgeResponse>(msg) {
                            // Find the matching BridgeOutput and resolve its pending response
                            let reg = state.outputs.lock().await;
                            // Search all bridge outputs for this response ID
                            for device_id in registered_devices.lock().await.iter() {
                                if let Some(output) = reg.get(device_id) {
                                    let output = output.lock().await;
                                    // Downcast to BridgeOutput to access pending_responses
                                    // For now, we use a shared pending map
                                    drop(output);
                                }
                            }
                            // Store in a shared response map that BridgeOutput checks
                            let mut bridge_conns = state.bridge_responses.lock().await;
                            if let Some(tx) = bridge_conns.remove(&resp.id) {
                                let _ = tx.send(resp);
                            }
                        }
                    }
                    "bridge.status" => {
                        // Forward status to PlaybackManager for position tracking
                        // TODO: update zone position from bridge status reports
                    }
                    _ => {
                        warn!(msg_type = %msg_type, "unknown bridge message type");
                    }
                }
            }
            Some(Ok(axum::extract::ws::Message::Ping(_data))) => {
                // Ping handled by tungstenite automatically
            }
            Some(Ok(axum::extract::ws::Message::Close(_))) | None | Some(Err(_)) => {
                break;
            }
            _ => {}
        }
    }

    // Cleanup: remove all bridge outputs, set zones offline
    connected.store(false, Ordering::Relaxed);
    writer_handle.abort();

    let devices = registered_for_cleanup.lock().await;
    let mut reg = state.outputs.lock().await;
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    for device_id in devices.iter() {
        reg.remove(device_id);
        let _ = zone_repo.set_online_by_device(device_id, false);
    }

    info!(
        bridge_id = %bridge_id,
        devices = devices.len(),
        "bridge disconnected, cleaned up"
    );
}

async fn handle_devices(
    state: &AppState,
    bridge_id: &str,
    devices: &[BridgeDevice],
    command_tx: &mpsc::Sender<BridgeCommand>,
    connected: &Arc<AtomicBool>,
    registered: &Arc<Mutex<Vec<String>>>,
) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let existing_zones = zone_repo.list().unwrap_or_default();

    for dev in devices {
        let full_id = format!("bridge:{bridge_id}:{}", dev.id);
        let output = BridgeOutput::new(
            dev.name.clone(),
            full_id.clone(),
            dev.device_type.clone(),
            bridge_id.to_owned(),
            command_tx.clone(),
            connected.clone(),
        );

        // Register the pending responses map in AppState for response routing
        // (BridgeOutput's pending map is internal — we route via bridge_responses)

        let mut reg = state.outputs.lock().await;
        reg.register(Box::new(output));
        drop(reg);

        registered.lock().await.push(full_id.clone());

        // Auto-create zone if not exists
        let already = existing_zones
            .iter()
            .any(|z| z.output_device_id.as_deref() == Some(&full_id));
        if already {
            let _ = zone_repo.set_online_by_device(&full_id, true);
            info!(name = %dev.name, id = %full_id, "bridge zone reconnected");
        } else {
            let name_taken = existing_zones.iter().any(|z| z.name == dev.name);
            if !name_taken {
                if let Ok(zid) = zone_repo.create(&dev.name, Some(&dev.device_type), Some(&full_id))
                {
                    info!(name = %dev.name, zone_id = zid, "bridge zone created");
                }
            }
        }

        state.event_bus.emit(
            "device.discovered",
            serde_json::json!({
                "id": full_id,
                "name": dev.name,
                "type": dev.device_type,
                "bridge": bridge_id,
            }),
        );
    }
}
