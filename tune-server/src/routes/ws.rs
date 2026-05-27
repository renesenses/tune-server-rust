use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(ws_handler))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn matches_pattern(event_type: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.ends_with(".*") {
        let prefix = &pattern[..pattern.len() - 2];
        return event_type.starts_with(prefix);
    }
    event_type == pattern
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.playback.subscribe();
    let mut event_rx = state.event_bus.subscribe();
    let mut patterns: Vec<String> = vec!["*".to_string()];

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        let event_type = format!("playback.{}", ev.event);

                        if !patterns.iter().any(|p| matches_pattern(&event_type, p)) {
                            continue;
                        }

                        let mut data = ev.data.clone();
                        if let Some(obj) = data.as_object_mut() {
                            obj.insert("zone_id".into(), serde_json::json!(ev.zone_id));
                        }
                        let ws_event = serde_json::json!({
                            "type": event_type,
                            "data": data,
                        });
                        let json = serde_json::to_string(&ws_event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(ev) => {
                        if !patterns.iter().any(|p| matches_pattern(&ev.event_type, p)) {
                            continue;
                        }
                        let ws_event = serde_json::json!({
                            "type": ev.event_type,
                            "data": ev.data,
                        });
                        let json = serde_json::to_string(&ws_event).unwrap_or_default();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(subs) = cmd.get("subscribe").and_then(|v| v.as_array()) {
                                patterns = subs.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect();
                                if patterns.is_empty() {
                                    patterns.push("*".to_string());
                                }
                                let ack = serde_json::json!({"type": "subscribed", "patterns": &patterns});
                                let _ = socket.send(Message::Text(
                                    serde_json::to_string(&ack).unwrap_or_default().into()
                                )).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
