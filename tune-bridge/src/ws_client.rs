use std::sync::Arc;

use axum::extract::WebSocketUpgrade;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::{info, warn};

use crate::state::RelayState;

#[derive(Deserialize)]
pub struct ClientQuery {
    token: Option<String>,
}

pub async fn ws_client_handler(
    ws: WebSocketUpgrade,
    Path(server_id): Path<String>,
    Query(query): Query<ClientQuery>,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    let token = query.token.unwrap_or_default();
    match state.server_for_token(&token) {
        Some(sid) if sid == server_id => {}
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    }

    if !state.servers.contains_key(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    ws.on_upgrade(move |socket| handle_client_ws(socket, server_id, state))
        .into_response()
}

async fn handle_client_ws(socket: WebSocket, server_id: String, state: Arc<RelayState>) {
    let client_id = uuid::Uuid::new_v4().to_string();
    info!(client_id = %client_id, server_id = %server_id, "remote client connected");

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Notify server of new client
    if let Some(conn) = state.servers.get(&server_id) {
        let msg = serde_json::json!({
            "type": "relay.client_connected",
            "client_id": client_id,
        });
        let _ = conn.ws_tx.send(msg.to_string()).await;
    }

    // For now: forward subscription requests from client to server,
    // and relay events from server to client.
    // The server will tag events with client_id so the relay can route them.
    loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(text))) => {
                // Forward client messages to server (subscribe patterns, etc.)
                if let Some(conn) = state.servers.get(&server_id) {
                    let wrapped = serde_json::json!({
                        "type": "relay.client_message",
                        "client_id": client_id,
                        "message": text.to_string(),
                    });
                    let _ = conn.ws_tx.send(wrapped.to_string()).await;
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws_tx.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
            _ => {}
        }
    }

    // Notify server of client disconnect
    if let Some(conn) = state.servers.get(&server_id) {
        let msg = serde_json::json!({
            "type": "relay.client_disconnected",
            "client_id": client_id,
        });
        let _ = conn.ws_tx.send(msg.to_string()).await;
    }

    info!(client_id = %client_id, server_id = %server_id, "remote client disconnected");
}
