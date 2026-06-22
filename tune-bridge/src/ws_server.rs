use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol;
use crate::state::RelayState;

pub async fn handle_server_ws(socket: WebSocket, state: Arc<RelayState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for relay.register
    let register: protocol::RelayRegister = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg_type = protocol::parse_message_type(&text);
                if msg_type.as_deref() == Some("relay.register") {
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(v) => match serde_json::from_value::<protocol::RelayRegister>(v) {
                            Ok(r) => break r,
                            Err(e) => {
                                warn!(error = %e, "invalid relay.register payload");
                                continue;
                            }
                        },
                        Err(_) => continue,
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws_tx.send(Message::Pong(data)).await;
            }
            None | Some(Err(_)) => return,
            _ => continue,
        }
    };

    info!(
        server_id = %register.server_id,
        server_name = %register.server_name,
        version = %register.version,
        "server registering"
    );

    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(256);

    if !state.register_server(
        register.server_id.clone(),
        register.server_name.clone(),
        register.bridge_token,
        msg_tx,
    ) {
        warn!("max servers reached, rejecting");
        let reject = serde_json::json!({
            "type": "relay.registered",
            "ok": false,
            "error": "max servers reached"
        });
        let _ = ws_tx.send(Message::Text(reject.to_string().into())).await;
        return;
    }

    let ack = protocol::RelayRegistered {
        msg_type: "relay.registered",
        ok: true,
        server_id: register.server_id.clone(),
    };
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await;

    info!(server_id = %register.server_id, "server registered");

    let server_id = register.server_id.clone();
    let server_id_writer = server_id.clone();
    let state_writer = state.clone();

    // Writer task: relay → server WS
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
        // If writer dies, unregister
        state_writer.unregister_server(&server_id_writer);
    });

    // Reader loop: server WS → relay
    let heartbeat_timeout = tokio::time::Duration::from_secs(90);
    let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    heartbeat_interval.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_server_message(&state, &server_id, &text).await;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        handle_server_binary(&state, &server_id, &data).await;
                    }
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                if let Some(conn) = state.servers.get(&server_id) {
                    if conn.last_heartbeat.elapsed() > heartbeat_timeout {
                        warn!(server_id = %server_id, "heartbeat timeout");
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    writer_handle.abort();
    state.unregister_server(&server_id);
    info!(server_id = %server_id, "server disconnected");
}

async fn handle_server_message(state: &RelayState, server_id: &str, text: &str) {
    let msg_type = match protocol::parse_message_type(text) {
        Some(t) => t,
        None => return,
    };

    match msg_type.as_str() {
        "relay.pong" => {
            if let Some(mut conn) = state.servers.get_mut(server_id) {
                conn.last_heartbeat = Instant::now();
            }
        }
        "relay.response" => {
            if let Ok(resp) = serde_json::from_str::<protocol::RelayResponse>(text) {
                resolve_pending(state, server_id, resp).await;
            }
        }
        "relay.event" => {
            // TODO Phase 1: forward to connected clients
        }
        "relay.stream_start" | "relay.stream_end" => {
            // TODO Phase 2: stream proxying
        }
        _ => {
            warn!(server_id = %server_id, msg_type = %msg_type, "unknown server message");
        }
    }
}

async fn handle_server_binary(_state: &RelayState, _server_id: &str, _data: &[u8]) {
    // TODO Phase 2: forward binary stream chunks to waiting HTTP responses
}

async fn resolve_pending(state: &RelayState, server_id: &str, resp: protocol::RelayResponse) {
    if let Some(conn) = state.servers.get(server_id) {
        let mut pending = conn.pending.lock().await;
        if let Some(tx) = pending.remove(&resp.id) {
            let _ = tx.send(crate::state::PendingResponse {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
            });
        }
    }
}
