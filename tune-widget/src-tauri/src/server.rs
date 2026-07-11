use futures_util::{SinkExt, StreamExt};
use tauri::{Emitter, Manager};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::AppState;

pub async fn ws_loop(app: tauri::AppHandle) {
    let mut backoff = 1u64;
    loop {
        let ws_url = {
            let state = app.state::<AppState>();
            let http_url = state.server_url.read().await;
            http_url.replace("http://", "ws://").replace("https://", "wss://") + "/ws"
        };
        match connect_async(&ws_url).await {
            Ok((ws, _)) => {
                backoff = 1;
                tracing::info!("ws_connected");
                let (mut write, mut read) = ws.split();

                let sub = serde_json::json!({"subscribe": ["playback.*", "zone.*"]});
                if write
                    .send(Message::Text(sub.to_string().into()))
                    .await
                    .is_err()
                {
                    continue;
                }

                while let Some(Ok(msg)) = read.next().await {
                    if let Message::Text(text) = msg {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                            let _ = app.emit("ws-event", val);
                        }
                    }
                }
                tracing::warn!("ws_disconnected");
            }
            Err(e) => {
                tracing::debug!(error = %e, backoff, "ws_connect_failed");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}
