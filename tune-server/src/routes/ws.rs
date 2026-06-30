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

/// Build the full current state sent to a client on connect (`type: "snapshot"`).
/// Merges persisted zone metadata (name/online/type/group) with live playback
/// state (transport, volume, now-playing, queue) so the client renders the
/// truth without polling.
async fn build_snapshot(state: &AppState) -> serde_json::Value {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    #[cfg(feature = "local-audio")]
    let audio_backend =
        tune_core::outputs::local::active_backend_name(&state.config.local_audio_backend);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    let devices = state.scanner.lock().await.devices().await;
    let mut zone_snaps = Vec::with_capacity(zones.len());
    for z in &zones {
        let zid = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zid).await;
        let renderer_label = z
            .output_device_id
            .as_deref()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
        let signal_path = crate::routes::zones::build_signal_path_pub(
            &ps,
            z,
            &state.backend,
            renderer_label,
            audio_backend,
        );
        zone_snaps.push(serde_json::json!({
            "zone_id": zid,
            "name": z.name,
            "online": z.online,
            "output_type": z.output_type,
            "group_id": z.group_id,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "volume": ps.volume,
            "muted": ps.muted,
            "shuffle": ps.shuffle,
            "repeat": ps.repeat,
            "position_ms": ps.position_ms,
            "queue_position": ps.queue_position,
            "queue_length": ps.queue_length,
            "now_playing": ps.now_playing,
            "signal_path": signal_path,
        }));
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: serde_json::Value = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!([]));

    serde_json::json!({
        "type": "snapshot",
        "data": { "zones": zone_snaps, "groups": groups },
    })
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.playback.subscribe();
    let mut event_rx = state.event_bus.subscribe();
    let mut patterns: Vec<String> = vec!["*".to_string()];
    let mut ping_interval = interval(PING_INTERVAL);
    ping_interval.tick().await;
    let mut last_scan_progress = std::time::Instant::now() - Duration::from_secs(10);

    // Snapshot-on-connect: hand the client the full current state up front so
    // it has the truth immediately, instead of a blank UI until the next event
    // (or a separate REST round-trip). Subscriptions above are already live, so
    // any change during snapshot building is buffered and delivered as a delta.
    {
        let snapshot = build_snapshot(&state).await;
        let json = serde_json::to_string(&snapshot).unwrap_or_default();
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

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
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("WebSocket broadcast closed, resubscribing");
                        rx = state.playback.subscribe();
                        continue;
                    }
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
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("WebSocket event_bus broadcast closed, resubscribing");
                        event_rx = state.event_bus.subscribe();
                        continue;
                    }
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
