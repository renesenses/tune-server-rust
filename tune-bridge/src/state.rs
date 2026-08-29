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
    /// Verificateur d'eligibilite premium, interroge AVANT tout
    /// enregistrement. Voir `crate::licence`.
    pub licences: std::sync::Arc<crate::licence::Licences>,
    pub max_servers: usize,
    pub max_clients_per_server: usize,
    pub max_streams_per_server: usize,
}

impl RelayState {
    pub fn new() -> Self {
        Self {
            servers: DashMap::new(),
            tokens: DashMap::new(),
            licences: crate::licence::Licences::depuis_environnement(),
            max_servers: 100,
            max_clients_per_server: 10,
            max_streams_per_server: 5,
        }
    }

    /// Register (or re-register) a server connection.
    ///
    /// Security: `server_id` and `bridge_token` are both chosen by the client,
    /// so registration must not let a stranger overwrite an existing server's
    /// connection — otherwise an attacker who guesses a live `server_id` could
    /// take over its socket and receive the client traffic proxied to it. The
    /// `bridge_token` (already the client-facing bearer secret used by
    /// `api_proxy`) doubles as the re-registration authenticator:
    ///
    /// - Replacing a *live* `server_id` is allowed only when the presented
    ///   token is the one already bound to it (a legit server reconnecting with
    ///   its persisted token). A mismatching token is rejected.
    /// - A *new* `server_id` may not claim a token already bound to a different
    ///   server, and is subject to the `max_servers` cap.
    ///
    /// Residual (needs JP's bridge design): a stranger can still *squat* an
    /// offline `server_id` (denial of service, not data hijack — clients
    /// authenticate with the real token, which the squatter lacks). Closing
    /// that requires authenticating the registrant itself (per-server signed
    /// challenge or a pre-shared bridge secret).
    pub fn register_server(
        &self,
        server_id: String,
        server_name: String,
        bridge_token: String,
        ws_tx: mpsc::Sender<String>,
    ) -> Result<(), &'static str> {
        // Reads clone out of the map (no guard held across the later inserts,
        // which on the same DashMap would deadlock).
        let already_live = self.servers.contains_key(&server_id);
        let token_owner = self.server_for_token(&bridge_token);

        if already_live {
            if token_owner.as_deref() != Some(server_id.as_str()) {
                return Err("server_id already registered");
            }
        } else {
            if let Some(owner) = &token_owner {
                if owner != &server_id {
                    return Err("bridge_token already bound to another server");
                }
            }
            if self.servers.len() >= self.max_servers {
                return Err("max servers reached");
            }
        }

        self.tokens.insert(bridge_token, server_id.clone());
        self.servers.insert(
            server_id.clone(),
            ServerConnection {
                server_id,
                server_name,
                ws_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                flux: Arc::new(Mutex::new(HashMap::new())),
                active_streams: AtomicU32::new(0),
                active_clients: AtomicU32::new(0),
                connected_at: Instant::now(),
                last_heartbeat: Instant::now(),
            },
        );
        Ok(())
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
    /// Puits des morceaux audio en vol, par identifiant de requete.
    ///
    /// Une reponse d'API se resout d'un coup : le `oneshot` de `pending`
    /// suffit. Un flux audio, non — il arrive en morceaux, longtemps apres
    /// l'en-tete. Le `oneshot` livre le debut de la reponse, et ce puits
    /// recoit la suite jusqu'a `relay.stream_end`.
    ///
    /// La carte vit dans la connexion : un serveur qui se deconnecte emporte
    /// ses puits, donc ferme les corps HTTP restes ouverts au lieu de les
    /// laisser pendre.
    pub flux: Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
    pub active_streams: AtomicU32,
    pub active_clients: AtomicU32,
    pub connected_at: Instant,
    pub last_heartbeat: Instant,
}

/// Corps d'une reponse relayee.
///
/// Le relais transporte deux choses de nature differente. Une reponse d'API
/// tient en memoire et arrive d'un bloc. Un flux audio ne tient pas en
/// memoire — un FLAC fait des dizaines de megaoctets — et arrive au fil de
/// l'eau. Les distinguer ici evite d'avoir a se demander, plus loin, si un
/// `Option<String>` vide veut dire « corps vide » ou « corps a venir ».
pub enum CorpsRelaye {
    /// Corps complet, deja recu.
    Entier(Option<String>),
    /// Morceaux a venir, jusqu'a la fermeture du canal.
    Morceaux(mpsc::Receiver<Vec<u8>>),
}

pub struct PendingResponse {
    pub status: u16,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: CorpsRelaye,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tx() -> mpsc::Sender<String> {
        mpsc::channel(1).0
    }

    fn state_with_cap(max_servers: usize) -> RelayState {
        RelayState {
            servers: DashMap::new(),
            tokens: DashMap::new(),
            // Sans jeton de service, `verifier` laisse passer : ces tests
            // portent sur les regles d'enregistrement, pas sur la licence.
            licences: crate::licence::Licences::depuis_environnement(),
            max_servers,
            max_clients_per_server: 10,
            max_streams_per_server: 5,
        }
    }

    fn reg(state: &RelayState, server_id: &str, token: &str) -> Result<(), &'static str> {
        state.register_server(
            server_id.to_string(),
            "name".to_string(),
            token.to_string(),
            dummy_tx(),
        )
    }

    #[test]
    fn new_server_registers_and_binds_token() {
        let s = state_with_cap(10);
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
        assert_eq!(s.server_for_token("tok-a").as_deref(), Some("srv-a"));
    }

    #[test]
    fn stranger_cannot_overwrite_live_server() {
        // The core P0: a different token must not take over a live server_id.
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(
            reg(&s, "srv-a", "attacker-token"),
            Err("server_id already registered"),
        );
        // The original binding is untouched.
        assert_eq!(s.server_for_token("tok-a").as_deref(), Some("srv-a"));
        assert_eq!(s.server_for_token("attacker-token"), None);
    }

    #[test]
    fn legit_reconnect_with_same_token_replaces() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        // Same identity reconnecting (its persisted token) is allowed to replace.
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
        assert_eq!(s.servers.len(), 1);
    }

    #[test]
    fn token_bound_to_another_server_is_rejected() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(
            reg(&s, "srv-b", "tok-a"),
            Err("bridge_token already bound to another server"),
        );
    }

    #[test]
    fn unregister_frees_id_and_token_for_reconnect() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        s.unregister_server("srv-a");
        assert_eq!(s.server_for_token("tok-a"), None);
        // A fresh registration (even a new token) is fine once the id is free.
        assert!(reg(&s, "srv-a", "tok-a2").is_ok());
    }

    #[test]
    fn max_servers_is_enforced_for_new_ids() {
        let s = state_with_cap(1);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(reg(&s, "srv-b", "tok-b"), Err("max servers reached"));
        // But re-registering the existing one is not blocked by the cap.
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
    }
}
