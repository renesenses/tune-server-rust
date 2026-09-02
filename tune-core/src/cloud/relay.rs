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
                    let pong = serde_json::json!({"type": "relay.pong"}).to_string();
                    emettre_vers_le_relais(&self.ws_tx, pong).await;
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
                let pong = serde_json::json!({"type": "relay.pong"}).to_string();
                emettre_vers_le_relais(&self.ws_tx, pong).await;
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

                    emettre_vers_le_relais(&ws_tx, resp.to_string()).await;
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
                            let content_length = resp
                                .headers()
                                .get("content-length")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.parse::<u64>().ok());

                            let hdrs = entetes_de_flux(resp.headers());

                            let start_msg = serde_json::json!({
                                "type": "relay.stream_start",
                                "id": id,
                                "status": status,
                                "headers": hdrs,
                                "content_length": content_length,
                            });

                            emettre_vers_le_relais(&ws_tx, start_msg.to_string()).await;

                            use futures_util::StreamExt;
                            let mut stream = resp.bytes_stream();
                            while let Some(chunk) = stream.next().await {
                                match chunk {
                                    Ok(bytes) => {
                                        // Trame TEXTE `BINARY:<id>:<base64>`.
                                        // Le canal vers le relais ne porte que
                                        // du texte (`mpsc::Sender<String>`),
                                        // d'ou l'encodage. Une trame binaire
                                        // prefixee de l'identifiant etait
                                        // assemblee ici puis jetee sans etre
                                        // envoyee : elle laissait croire a un
                                        // second format de fil qui n'a jamais
                                        // existe.
                                        //
                                        // C'est ICI que la liaison lente se
                                        // fait sentir : ce `send` attend que le
                                        // relais ait de la place. Il attend
                                        // sans le verrou, sinon le `relay.pong`
                                        // ne partirait plus et la session
                                        // entiere serait coupee.
                                        emettre_vers_le_relais(
                                            &ws_tx,
                                            format!("BINARY:{}:{}", id, base64_encode(&bytes)),
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        warn!(id = %id, error = %e, "stream chunk error");
                                        break;
                                    }
                                }
                            }

                            let end_msg = serde_json::json!({"type": "relay.stream_end", "id": id});
                            emettre_vers_le_relais(&ws_tx, end_msg.to_string()).await;
                        }
                        Err(e) => {
                            warn!(id = %id, error = %e, "relay stream request failed");
                            let resp = serde_json::json!({
                                "type": "relay.stream_start",
                                "id": id,
                                "status": 502,
                                "headers": {},
                            });
                            emettre_vers_le_relais(&ws_tx, resp.to_string()).await;
                        }
                    }
                });
            }
            _ => {}
        }
    }
}

/// Emet une trame vers le relais SANS retenir le verrou du canal pendant
/// l'attente.
///
/// `ws_tx` est partage par tous les emetteurs du client : les morceaux audio
/// de chaque flux en cours, les reponses d'API, et le `relay.pong` du
/// battement de coeur. Le canal est borne (256) et c'est voulu : quand le
/// navigateur distant lit moins vite que le disque ne debite — une
/// bibliotheque audiophile en FLAC 24/96 sur une liaison mobile, exactement le
/// cas de l'ecoute a distance — `send` attend. C'est la contre-pression, elle
/// evite de charger un album entier en memoire.
///
/// Attendre **le verrou en main** transforme cette contre-pression en panne
/// generale. La tache du flux sature garde le verrou pendant toute l'attente ;
/// le `relay.pong` ne peut plus etre emis ; le relais ne voit plus de
/// battement et coupe la connexion au bout de 90 s (`heartbeat_timeout`,
/// `tune-bridge/src/ws_server.rs`). Toute l'ecoute distante tombe — les autres
/// flux, l'API, la session entiere — parce qu'UN auditeur a une liaison lente.
///
/// Le `Sender` est donc clone hors du verrou, et l'attente se fait sans lui.
/// C'est la meme regle que `transmettre_morceau` applique deja de l'autre cote
/// du fil, cote relais.
///
/// L'ordre d'un flux donne est preserve : chaque flux est pompe par une seule
/// tache, qui attend un `send` avant d'entamer le suivant.
///
/// Rend `false` quand le canal est ferme (relais deconnecte).
pub(crate) async fn emettre_vers_le_relais(
    ws_tx: &Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
    trame: String,
) -> bool {
    // Le clone sort du verrou, le garde meurt ici : rien n'est tenu pendant le
    // `send` qui suit.
    let canal = { ws_tx.lock().await.clone() };
    match canal {
        Some(tx) => tx.send(trame).await.is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod emission_vers_le_relais_tests {
    use super::emettre_vers_le_relais;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};

    fn canal(
        capacite: usize,
    ) -> (
        Arc<Mutex<Option<mpsc::Sender<String>>>>,
        mpsc::Receiver<String>,
    ) {
        let (tx, rx) = mpsc::channel::<String>(capacite);
        (Arc::new(Mutex::new(Some(tx))), rx)
    }

    /// L'EPREUVE. Le canal est plein : l'emission suivante doit attendre — la
    /// contre-pression est voulue. Ce qui ne doit PAS arriver, c'est qu'elle
    /// attende en gardant le verrou : `ws_tx` est aussi le chemin du
    /// `relay.pong`, et un pong qui ne part plus fait couper la session
    /// entiere par le relais au bout de 90 s.
    ///
    /// Un auditeur sur liaison lente ne doit couter que son propre flux.
    #[tokio::test]
    async fn un_envoi_bloque_ne_retient_pas_le_verrou_du_canal() {
        let (ws_tx, _rx) = canal(1);

        // Saturer le canal : l'unique place est prise et personne ne lit.
        assert!(emettre_vers_le_relais(&ws_tx, "premier".to_string()).await);

        // Le morceau audio suivant ne peut plus passer : il va attendre.
        let mut bloque = Box::pin(emettre_vers_le_relais(
            &ws_tx,
            "BINARY:req-1:ZkxhQw==".to_string(),
        ));
        assert!(
            futures_util::poll!(&mut bloque).is_pending(),
            "le canal est plein : l'emission devait attendre",
        );

        // Pendant cette attente, le battement de coeur doit pouvoir emettre.
        assert!(
            ws_tx.try_lock().is_ok(),
            "le verrou est retenu pendant l'attente : le relay.pong ne peut \
             plus partir, le relais coupera la session au bout de 90 s",
        );
    }

    /// Le temoin positif de l'epreuve ci-dessus : tant qu'il reste de la
    /// place, l'emission aboutit sans attendre, et la trame arrive intacte.
    #[tokio::test]
    async fn la_trame_arrive_intacte_au_relais() {
        let (ws_tx, mut rx) = canal(4);
        assert!(emettre_vers_le_relais(&ws_tx, "BINARY:req-1:ZkxhQw==".to_string()).await);
        assert_eq!(rx.recv().await.as_deref(), Some("BINARY:req-1:ZkxhQw=="));
    }

    /// Deux flux, une seule place : le second attend, mais le verrou reste
    /// libre pour tous les autres emetteurs du client.
    #[tokio::test]
    async fn un_flux_sature_ne_bloque_pas_le_reste_du_client() {
        let (ws_tx, mut rx) = canal(1);
        assert!(emettre_vers_le_relais(&ws_tx, "BINARY:lent:AAAA".to_string()).await);

        let mut lent = Box::pin(emettre_vers_le_relais(
            &ws_tx,
            "BINARY:lent:BBBB".to_string(),
        ));
        assert!(futures_util::poll!(&mut lent).is_pending());

        // Le relais lit une trame : la place liberee doit profiter au flux en
        // attente, sans qu'aucun verrou n'ait ete retenu entre-temps.
        assert_eq!(rx.recv().await.as_deref(), Some("BINARY:lent:AAAA"));
        assert!(lent.await);
        assert_eq!(rx.recv().await.as_deref(), Some("BINARY:lent:BBBB"));
    }

    /// Relais deconnecte : `ws_tx` ne porte plus de canal. L'emission ne
    /// panique pas, elle dit non.
    #[tokio::test]
    async fn sans_canal_ouvert_lemission_echoue_sans_paniquer() {
        let ws_tx: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));
        assert!(!emettre_vers_le_relais(&ws_tx, "relay.pong".to_string()).await);
    }

    /// Canal ferme cote relais : l'echec est signale, pas avale en silence.
    #[tokio::test]
    async fn un_canal_ferme_est_signale_a_lappelant() {
        let (ws_tx, rx) = canal(1);
        drop(rx);
        assert!(!emettre_vers_le_relais(&ws_tx, "relay.pong".to_string()).await);
    }
}

/// En-tetes qu'un flux relaye doit emporter jusqu'au navigateur.
///
/// Seul `content-type` traversait. Manquait donc tout ce qui permet a une
/// balise `<audio>` de se situer dans le morceau : sans `content-range`, une
/// reponse 206 est invalide et le lecteur abandonne ; sans `accept-ranges`, il
/// ne tente meme pas de se deplacer ; sans `content-length`, il n'a ni duree
/// ni barre de progression.
///
/// Le repli `application/octet-stream` est conserve : un flux sans type
/// declare vaut mieux qu'un flux sans en-tete du tout.
pub(crate) fn entetes_de_flux(
    entetes: &reqwest::header::HeaderMap,
) -> serde_json::Map<String, serde_json::Value> {
    let mut sortie = serde_json::Map::new();
    let type_contenu = entetes
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");
    sortie.insert(
        "content-type".to_string(),
        serde_json::Value::String(type_contenu.to_string()),
    );
    for nom in ["content-length", "content-range", "accept-ranges"] {
        if let Some(valeur) = entetes.get(nom).and_then(|v| v.to_str().ok()) {
            sortie.insert(
                nom.to_string(),
                serde_json::Value::String(valeur.to_string()),
            );
        }
    }
    sortie
}

#[cfg(test)]
mod entetes_de_flux_tests {
    use super::entetes_de_flux;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn entetes(paires: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (nom, valeur) in paires {
            h.insert(*nom, HeaderValue::from_str(valeur).unwrap());
        }
        h
    }

    /// Le cas qui rendait le deplacement impossible : une 206 sans
    /// `content-range` est invalide, le lecteur abandonne la lecture.
    #[test]
    fn une_reponse_partielle_emporte_son_content_range() {
        let sortie = entetes_de_flux(&entetes(&[
            ("content-type", "audio/flac"),
            ("content-range", "bytes 100-199/5000"),
            ("accept-ranges", "bytes"),
            ("content-length", "100"),
        ]));
        assert_eq!(sortie["content-type"], "audio/flac");
        assert_eq!(sortie["content-range"], "bytes 100-199/5000");
        assert_eq!(sortie["accept-ranges"], "bytes");
        assert_eq!(sortie["content-length"], "100");
    }

    /// Un en-tete absent ne doit pas etre invente : mieux vaut un champ
    /// manquant qu'un `content-range` faux.
    #[test]
    fn aucun_en_tete_nest_fabrique() {
        let sortie = entetes_de_flux(&entetes(&[("content-type", "audio/flac")]));
        assert_eq!(sortie.len(), 1);
        assert!(!sortie.contains_key("content-range"));
        assert!(!sortie.contains_key("accept-ranges"));
        assert!(!sortie.contains_key("content-length"));
    }

    #[test]
    fn sans_type_declare_le_repli_est_generique() {
        let sortie = entetes_de_flux(&entetes(&[]));
        assert_eq!(sortie["content-type"], "application/octet-stream");
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
            // Never log the token value — it is the client-facing bearer secret
            // used to reach this server through the relay.
            info!("generated new bridge token");
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
