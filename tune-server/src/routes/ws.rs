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

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.playback.subscribe();

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        let mut data = ev.data.clone();
                        if let Some(obj) = data.as_object_mut() {
                            obj.insert("zone_id".into(), serde_json::json!(ev.zone_id));
                        }
                        let ws_event = serde_json::json!({
                            "type": format!("playback.{}", ev.event),
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
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data)))
                        if socket.send(Message::Pong(data)).await.is_err() => {
                            break;
                        }
                    _ => {}
                }
            }
        }
    }
}
