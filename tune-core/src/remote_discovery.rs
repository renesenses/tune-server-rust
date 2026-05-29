use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const TUNE_SSDP_ST: &str = "urn:mozaiklabs-fr:service:TuneServer:1";
const SSDP_MULTICAST: &str = "239.255.255.250:1900";
const PEER_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerServer {
    pub server_id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub version: String,
    pub last_seen: u64,
}

pub struct PeerRegistry {
    peers: Mutex<HashMap<String, PeerEntry>>,
    server_id: String,
    server_name: String,
    port: u16,
}

struct PeerEntry {
    peer: PeerServer,
    seen_at: Instant,
}

impl PeerRegistry {
    pub fn new(server_id: String, server_name: String, port: u16) -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            server_id,
            server_name,
            port,
        }
    }

    pub async fn register_peer(&self, peer: PeerServer) {
        if peer.server_id == self.server_id {
            return;
        }
        let id = peer.server_id.clone();
        let name = peer.name.clone();
        self.peers.lock().await.insert(
            id.clone(),
            PeerEntry {
                peer,
                seen_at: Instant::now(),
            },
        );
        debug!(id = %id, name = %name, "peer_registered");
    }

    pub async fn list_peers(&self) -> Vec<PeerServer> {
        let peers = self.peers.lock().await;
        peers
            .values()
            .filter(|e| e.seen_at.elapsed() < PEER_TTL)
            .map(|e| e.peer.clone())
            .collect()
    }

    pub async fn get_peer(&self, server_id: &str) -> Option<PeerServer> {
        let peers = self.peers.lock().await;
        peers
            .get(server_id)
            .filter(|e| e.seen_at.elapsed() < PEER_TTL)
            .map(|e| e.peer.clone())
    }

    pub async fn prune_stale(&self) -> usize {
        let mut peers = self.peers.lock().await;
        let before = peers.len();
        peers.retain(|_, e| e.seen_at.elapsed() < PEER_TTL);
        before - peers.len()
    }

    pub fn ssdp_notify_message(&self, local_ip: &str) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             NT: {TUNE_SSDP_ST}\r\n\
             NTS: ssdp:alive\r\n\
             USN: uuid:{server_id}::{TUNE_SSDP_ST}\r\n\
             LOCATION: http://{local_ip}:{port}/api/system/info\r\n\
             SERVER: TuneServer/{version}\r\n\
             CACHE-CONTROL: max-age=300\r\n\
             X-TUNE-NAME: {name}\r\n\
             \r\n",
            server_id = self.server_id,
            port = self.port,
            version = crate::version(),
            name = self.server_name,
        )
    }

    pub fn ssdp_search_message() -> String {
        format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 3\r\n\
             ST: {TUNE_SSDP_ST}\r\n\
             \r\n"
        )
    }

    pub async fn send_discovery(&self) -> Result<Vec<PeerServer>, String> {
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| format!("udp bind: {e}"))?;

        let search = Self::ssdp_search_message();
        let target: SocketAddr = SSDP_MULTICAST
            .parse()
            .map_err(|e| format!("parse: {e}"))?;

        socket
            .send_to(search.as_bytes(), target)
            .await
            .map_err(|e| format!("send: {e}"))?;

        let mut buf = vec![0u8; 2048];
        let mut found = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                Ok(Ok((len, addr))) => {
                    let response = String::from_utf8_lossy(&buf[..len]);
                    if let Some(peer) = parse_ssdp_response(&response, &addr) {
                        if peer.server_id != self.server_id {
                            self.register_peer(peer.clone()).await;
                            found.push(peer);
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "ssdp_recv_error");
                    break;
                }
                Err(_) => break,
            }
        }

        info!(found = found.len(), "peer_discovery_complete");
        Ok(found)
    }

    pub fn spawn_announcer(self: std::sync::Arc<Self>, local_ip: String) {
        tokio::spawn(async move {
            loop {
                if let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await {
                    let msg = self.ssdp_notify_message(&local_ip);
                    if let Ok(target) = SSDP_MULTICAST.parse::<SocketAddr>() {
                        let _ = socket.send_to(msg.as_bytes(), target).await;
                    }
                }
                self.prune_stale().await;
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }
}

fn parse_ssdp_response(response: &str, addr: &SocketAddr) -> Option<PeerServer> {
    if !response.contains(TUNE_SSDP_ST) {
        return None;
    }

    let headers = parse_headers(response);

    let usn = headers.get("usn")?;
    let server_id = usn
        .strip_prefix("uuid:")
        .and_then(|s| s.split("::").next())
        .unwrap_or(usn)
        .to_string();

    let location = headers.get("location")?;
    let server_version = headers
        .get("server")
        .and_then(|s| s.strip_prefix("TuneServer/"))
        .unwrap_or("unknown")
        .to_string();

    let name = headers
        .get("x-tune-name")
        .cloned()
        .unwrap_or_else(|| format!("Tune Server @ {}", addr.ip()));

    let port = location
        .split(':')
        .nth(2)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Some(PeerServer {
        server_id,
        name,
        address: addr.ip().to_string(),
        port,
        version: server_version,
        last_seen: now,
    })
}

fn parse_headers(response: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for line in response.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(
                key.trim().to_lowercase(),
                value.trim().to_string(),
            );
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_message_format() {
        let msg = PeerRegistry::ssdp_search_message();
        assert!(msg.contains("M-SEARCH"));
        assert!(msg.contains(TUNE_SSDP_ST));
        assert!(msg.contains("ssdp:discover"));
    }

    #[test]
    fn notify_message_format() {
        let registry = PeerRegistry::new("test-id".into(), "TestServer".into(), 3000);
        let msg = registry.ssdp_notify_message("192.168.1.10");
        assert!(msg.contains("NOTIFY"));
        assert!(msg.contains("ssdp:alive"));
        assert!(msg.contains("test-id"));
        assert!(msg.contains("TestServer"));
        assert!(msg.contains("192.168.1.10:3000"));
    }

    #[test]
    fn parse_ssdp_valid() {
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             USN: uuid:abc-123::{TUNE_SSDP_ST}\r\n\
             LOCATION: http://192.168.1.20:3000/api/system/info\r\n\
             SERVER: TuneServer/1.2.3\r\n\
             X-TUNE-NAME: Living Room\r\n\
             \r\n"
        );
        let addr: SocketAddr = "192.168.1.20:1900".parse().unwrap();
        let peer = parse_ssdp_response(&response, &addr).unwrap();
        assert_eq!(peer.server_id, "abc-123");
        assert_eq!(peer.name, "Living Room");
        assert_eq!(peer.version, "1.2.3");
        assert_eq!(peer.port, 3000);
    }

    #[test]
    fn parse_ssdp_wrong_st() {
        let response = "HTTP/1.1 200 OK\r\nST: upnp:rootdevice\r\n\r\n";
        let addr: SocketAddr = "192.168.1.1:1900".parse().unwrap();
        assert!(parse_ssdp_response(response, &addr).is_none());
    }

    #[tokio::test]
    async fn registry_operations() {
        let registry = PeerRegistry::new("self-id".into(), "Self".into(), 3000);

        let peer = PeerServer {
            server_id: "other-id".into(),
            name: "Other".into(),
            address: "192.168.1.2".into(),
            port: 3000,
            version: "1.0.0".into(),
            last_seen: 0,
        };

        registry.register_peer(peer).await;
        let peers = registry.list_peers().await;
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].server_id, "other-id");
    }

    #[tokio::test]
    async fn self_discovery_ignored() {
        let registry = PeerRegistry::new("self-id".into(), "Self".into(), 3000);
        let peer = PeerServer {
            server_id: "self-id".into(),
            name: "Self".into(),
            address: "127.0.0.1".into(),
            port: 3000,
            version: "1.0.0".into(),
            last_seen: 0,
        };

        registry.register_peer(peer).await;
        assert!(registry.list_peers().await.is_empty());
    }
}
