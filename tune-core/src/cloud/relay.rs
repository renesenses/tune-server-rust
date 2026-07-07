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
    pub local_port: u16,
    connected: Arc<AtomicBool>,
    ws_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
    http_client: reqwest::Client,
}

impl RelayClient {
    pub fn new(
        server_id: String,
        bridge_token: String,
        relay_url: String,
        local_port: u16,
    ) -> Self {
        let http_client = crate::http::client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("http client");
        Self {
            server_id,
            bridge_token,
            relay_url,
            local_port,
            connected: Arc::new(AtomicBool::new(false)),
            ws_tx: Arc::new(tokio::sync::Mutex::new(None)),
            http_client,
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
                let id = v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
                let path = v.get("path").and_then(|p| p.as_str()).unwrap_or("/");
                let body = v
                    .get("body")
                    .and_then(|b| b.as_str())
                    .map(|s| s.to_string());
                let headers = v.get("headers").and_then(|h| h.as_object()).cloned();

                let url = format!("http://127.0.0.1:{}{}", self.local_port, path);
                let mut req = match method {
                    "POST" => self.http_client.post(&url),
                    "PUT" => self.http_client.put(&url),
                    "DELETE" => self.http_client.delete(&url),
                    "PATCH" => self.http_client.patch(&url),
                    _ => self.http_client.get(&url),
                };

                if let Some(hdrs) = headers {
                    for (k, val) in &hdrs {
                        if let Some(v) = val.as_str() {
                            req = req.header(k.as_str(), v);
                        }
                    }
                }
                if let Some(b) = body {
                    req = req.body(b);
                }

                let ws_tx = self.ws_tx.clone();
                let id_clone = id.clone();
                tokio::spawn(async move {
                    let (status, resp_headers, resp_body) = match req.send().await {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let ct = resp
                                .headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("application/json")
                                .to_string();
                            let body = resp.text().await.unwrap_or_default();
                            let mut hdrs = serde_json::Map::new();
                            hdrs.insert("content-type".to_string(), serde_json::Value::String(ct));
                            (status, hdrs, body)
                        }
                        Err(e) => {
                            warn!(id = %id_clone, error = %e, "relay local dispatch failed");
                            let mut hdrs = serde_json::Map::new();
                            hdrs.insert(
                                "content-type".to_string(),
                                serde_json::Value::String("application/json".into()),
                            );
                            (
                                502,
                                hdrs,
                                format!("{{\"error\": \"local dispatch failed: {e}\"}}"),
                            )
                        }
                    };

                    let resp = serde_json::json!({
                        "type": "relay.response",
                        "id": id_clone,
                        "status": status,
                        "headers": resp_headers,
                        "body": resp_body,
                    });

                    let tx = ws_tx.lock().await;
                    if let Some(tx) = tx.as_ref() {
                        let _ = tx.send(resp.to_string()).await;
                    }
                });
            }
            "relay.stream_request" => {
                let id = v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let stream_id = v
                    .get("stream_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let range = v
                    .get("range")
                    .and_then(|r| r.as_str())
                    .map(|s| s.to_string());

                let url = format!("http://127.0.0.1:{}/stream/{}", self.local_port, stream_id);
                let ws_tx = self.ws_tx.clone();
                let http = self.http_client.clone();

                tokio::spawn(async move {
                    let mut req = http.get(&url);
                    if let Some(r) = range {
                        req = req.header("range", r);
                    }

                    match req.send().await {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let ct = resp
                                .headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("application/octet-stream")
                                .to_string();
                            let content_length = resp
                                .headers()
                                .get("content-length")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.parse::<u64>().ok());

                            let mut hdrs = serde_json::Map::new();
                            hdrs.insert("content-type".to_string(), serde_json::Value::String(ct));

                            let start_msg = serde_json::json!({
                                "type": "relay.stream_start",
                                "id": id,
                                "status": status,
                                "headers": hdrs,
                                "content_length": content_length,
                            });

                            {
                                let tx = ws_tx.lock().await;
                                if let Some(tx) = tx.as_ref() {
                                    let _ = tx.send(start_msg.to_string()).await;
                                }
                            }

                            use futures_util::StreamExt;
                            let mut stream = resp.bytes_stream();
                            while let Some(chunk) = stream.next().await {
                                match chunk {
                                    Ok(bytes) => {
                                        let tx = ws_tx.lock().await;
                                        if let Some(tx) = tx.as_ref() {
                                            // Binary frame: first 36 bytes = request id (UUID), rest = audio data
                                            let mut frame = Vec::with_capacity(36 + bytes.len());
                                            frame.extend_from_slice(
                                                id.as_bytes().get(..36).unwrap_or(id.as_bytes()),
                                            );
                                            // Pad to 36 if id shorter
                                            while frame.len() < 36 {
                                                frame.push(0);
                                            }
                                            frame.extend_from_slice(&bytes);
                                            // Send as JSON with base64 would be too slow,
                                            // so we encode the stream_id in the frame header
                                            let _ = tx
                                                .send(format!(
                                                    "BINARY:{}:{}",
                                                    id,
                                                    base64_encode(&bytes)
                                                ))
                                                .await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(id = %id, error = %e, "stream chunk error");
                                        break;
                                    }
                                }
                            }

                            let end_msg = serde_json::json!({"type": "relay.stream_end", "id": id});
                            let tx = ws_tx.lock().await;
                            if let Some(tx) = tx.as_ref() {
                                let _ = tx.send(end_msg.to_string()).await;
                            }
                        }
                        Err(e) => {
                            warn!(id = %id, error = %e, "relay stream request failed");
                            let resp = serde_json::json!({
                                "type": "relay.stream_start",
                                "id": id,
                                "status": 502,
                                "headers": {},
                            });
                            let tx = ws_tx.lock().await;
                            if let Some(tx) = tx.as_ref() {
                                let _ = tx.send(resp.to_string()).await;
                            }
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Tune Server".to_string())
}

pub fn spawn_relay_client(settings: &SettingsRepo, local_port: u16) -> Option<Arc<RelayClient>> {
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

    let client = Arc::new(RelayClient::new(
        server_id,
        bridge_token,
        relay_url,
        local_port,
    ));
    client.clone().spawn();
    Some(client)
}
