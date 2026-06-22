use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::db::settings_repo::SettingsRepo;

pub struct RelayClient {
    pub server_id: String,
    pub bridge_token: String,
    pub relay_url: String,
    connected: Arc<AtomicBool>,
    ws_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
}

impl RelayClient {
    pub fn new(server_id: String, bridge_token: String, relay_url: String) -> Self {
        Self {
            server_id,
            bridge_token,
            relay_url,
            connected: Arc::new(AtomicBool::new(false)),
            ws_tx: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn spawn(self: Arc<Self>) {
        let client = self.clone();
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                info!(
                    relay_url = %client.relay_url,
                    server_id = %client.server_id,
                    attempt = attempt,
                    "connecting to relay"
                );

                match client.connect_and_run().await {
                    Ok(()) => {
                        info!("relay connection closed gracefully");
                    }
                    Err(e) => {
                        warn!(error = %e, "relay connection failed");
                    }
                }

                client.connected.store(false, Ordering::Relaxed);
                *client.ws_tx.lock().await = None;

                attempt += 1;
                let backoff = Duration::from_secs(std::cmp::min(
                    1u64.saturating_mul(1 << attempt.min(6)),
                    60,
                ));
                info!(
                    backoff_secs = backoff.as_secs(),
                    "reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
            }
        });
    }

    async fn connect_and_run(self: &Arc<Self>) -> Result<(), String> {
        use tokio_tungstenite::tungstenite;

        let (ws_stream, _) = tokio_tungstenite::connect_async(&self.relay_url)
            .await
            .map_err(|e| format!("ws connect: {e}"))?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Send relay.register
        let register = serde_json::json!({
            "type": "relay.register",
            "server_id": self.server_id,
            "server_name": hostname(),
            "version": crate::version(),
            "bridge_token": self.bridge_token,
        });
        ws_tx
            .send(tungstenite::Message::Text(register.to_string().into()))
            .await
            .map_err(|e| format!("ws send register: {e}"))?;

        // Wait for relay.registered
        let ack = ws_rx
            .next()
            .await
            .ok_or("connection closed before ack")?
            .map_err(|e| format!("ws read ack: {e}"))?;

        if let tungstenite::Message::Text(text) = ack {
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("parse ack: {e}"))?;
            if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                let err = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("rejected");
                return Err(format!("relay rejected: {err}"));
            }
        }

        info!(server_id = %self.server_id, "registered with relay");
        self.connected.store(true, Ordering::Relaxed);

        let (msg_tx, mut msg_rx) = mpsc::channel::<String>(256);
        *self.ws_tx.lock().await = Some(msg_tx);

        // Writer: forward outbound messages to WS
        let writer_connected = self.connected.clone();
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                if ws_tx
                    .send(tungstenite::Message::Text(msg.into()))
                    .await
                    .is_err()
                {
                    writer_connected.store(false, Ordering::Relaxed);
                    break;
                }
            }
        });

        // Reader: handle incoming messages from relay
        loop {
            match ws_rx.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    self.handle_message(&text).await;
                }
                Some(Ok(tungstenite::Message::Ping(data))) => {
                    let tx = self.ws_tx.lock().await;
                    if let Some(tx) = tx.as_ref() {
                        let pong = serde_json::json!({"type": "relay.pong"}).to_string();
                        let _ = tx.send(pong).await;
                    }
                    drop(tx);
                    let _ = data; // ping data handled by tungstenite
                }
                Some(Ok(tungstenite::Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }

        writer_handle.abort();
        Ok(())
    }

    async fn handle_message(&self, text: &str) {
        let v: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        let msg_type = match v.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => return,
        };

        match msg_type {
            "relay.ping" => {
                let tx = self.ws_tx.lock().await;
                if let Some(tx) = tx.as_ref() {
                    let pong = serde_json::json!({"type": "relay.pong"}).to_string();
                    let _ = tx.send(pong).await;
                }
            }
            "relay.request" => {
                // TODO Phase 1: dispatch to local router via tower::Service
                let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
                warn!(id = %id, "relay.request received but dispatch not yet implemented");
                let tx = self.ws_tx.lock().await;
                if let Some(tx) = tx.as_ref() {
                    let resp = serde_json::json!({
                        "type": "relay.response",
                        "id": id,
                        "status": 501,
                        "headers": {},
                        "body": "{\"error\": \"relay dispatch not yet implemented\"}"
                    });
                    let _ = tx.send(resp.to_string()).await;
                }
            }
            "relay.stream_request" => {
                // TODO Phase 2: pipe local stream data back
                warn!("relay.stream_request not yet implemented");
            }
            _ => {}
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Tune Server".to_string())
}

pub fn spawn_relay_client(settings: &SettingsRepo) -> Option<Arc<RelayClient>> {
    let enabled = settings
        .get("bridge_enabled")
        .ok()
        .flatten()
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes"))
        .unwrap_or(false);

    if !enabled {
        // Also check env var
        let env_enabled = std::env::var("TUNE_BRIDGE_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);
        if !env_enabled {
            info!("bridge relay disabled");
            return None;
        }
    }

    let relay_url = settings
        .get("bridge_url")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_BRIDGE_URL").ok())
        .unwrap_or_else(|| "wss://bridge.mozaiklabs.fr/ws/server".to_string());

    let bridge_token = settings
        .get("bridge_token")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_BRIDGE_TOKEN").ok());

    let bridge_token = match bridge_token {
        Some(t) if !t.is_empty() => t,
        _ => {
            let token = uuid::Uuid::new_v4().to_string();
            let _ = settings.set("bridge_token", &token);
            info!(token = %token, "generated new bridge token");
            token
        }
    };

    let server_id = crate::cloud::telemetry::TelemetryReporter::get_or_create_server_id(settings);

    let client = Arc::new(RelayClient::new(server_id, bridge_token, relay_url));
    client.clone().spawn();
    Some(client)
}
