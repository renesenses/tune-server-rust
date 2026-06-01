use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::time::interval;

use crate::state::AppState;

const PING_INTERVAL: Duration = Duration::from_secs(15);

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(ws_handler))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
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
    let mut ping_interval = interval(PING_INTERVAL);
    ping_interval.tick().await;
    let mut last_scan_progress = std::time::Instant::now() - Duration::from_secs(10);

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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket playback broadcast lagged, skipped {n} messages");
                        continue;
                    }
                    Err(_) => break,
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(ev) => {
                        if !patterns.iter().any(|p| matches_pattern(&ev.event_type, p)) {
                            continue;
                        }
                        // Throttle scan progress events to max 1 per 2s per client
                        if ev.event_type == "library.scan.progress" {
                            let now = std::time::Instant::now();
                            if now.duration_since(last_scan_progress) < Duration::from_secs(2) {
                                continue;
                            }
                            last_scan_progress = now;
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
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket event_bus broadcast lagged, skipped {n} messages");
                        continue;
                    }
                    Err(_) => break,
                }
            }
            _ = ping_interval.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) {
                            // Support both formats:
                            // v1: {"subscribe": ["pattern1", "pattern2"]}
                            // v2: {"action": "subscribe", "patterns": ["pattern1", "pattern2"]}
                            let subs = cmd.get("subscribe").and_then(|v| v.as_array())
                                .or_else(|| {
                                    if cmd.get("action").and_then(|v| v.as_str()) == Some("subscribe") {
                                        cmd.get("patterns").and_then(|v| v.as_array())
                                    } else {
                                        None
                                    }
                                });
                            if let Some(subs) = subs {
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
