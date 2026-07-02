// Bridge server state; some fields are populated by the not-yet-wired bridge
// phases (1-3) and read later — annotate rather than drop.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc, oneshot};

pub struct RelayState {
    pub servers: DashMap<String, ServerConnection>,
    pub tokens: DashMap<String, String>,
    pub max_servers: usize,
    pub max_clients_per_server: usize,
    pub max_streams_per_server: usize,
}

impl RelayState {
    pub fn new() -> Self {
        Self {
            servers: DashMap::new(),
            tokens: DashMap::new(),
            max_servers: 100,
            max_clients_per_server: 10,
            max_streams_per_server: 5,
        }
    }

    pub fn register_server(
        &self,
        server_id: String,
        server_name: String,
        bridge_token: String,
        ws_tx: mpsc::Sender<String>,
    ) -> bool {
        if self.servers.len() >= self.max_servers {
            return false;
        }
        self.tokens.insert(bridge_token, server_id.clone());
        self.servers.insert(
            server_id.clone(),
            ServerConnection {
                server_id,
                server_name,
                ws_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                active_streams: AtomicU32::new(0),
                active_clients: AtomicU32::new(0),
                connected_at: Instant::now(),
                last_heartbeat: Instant::now(),
            },
        );
        true
    }

    pub fn unregister_server(&self, server_id: &str) {
        if let Some((_, conn)) = self.servers.remove(server_id) {
            self.tokens.retain(|_, v| v != &conn.server_id);
        }
    }

    pub fn server_for_token(&self, token: &str) -> Option<String> {
        self.tokens.get(token).map(|v| v.clone())
    }
}

pub struct ServerConnection {
    pub server_id: String,
    pub server_name: String,
    pub ws_tx: mpsc::Sender<String>,
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingResponse>>>>,
    pub active_streams: AtomicU32,
    pub active_clients: AtomicU32,
    pub connected_at: Instant,
    pub last_heartbeat: Instant,
}

pub struct PendingResponse {
    pub status: u16,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: Option<String>,
}
