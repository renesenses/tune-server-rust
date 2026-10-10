use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

use super::ENTETE_RELAIS;

pub struct RelayClient {
    pub server_id: String,
    pub bridge_token: String,
    pub relay_url: String,
    pub local_port: u16,
    connected: Arc<AtomicBool>,
    ws_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
    http_client: reqwest::Client,
    /// La base des réglages, pour savoir si le greffon `circle` est installé
    /// (Tune Circle T4, #5327). `None` : aucune écoute de contact n'est
    /// servie — une absence ne vaut jamais une autorisation.
    reglages: Option<Arc<dyn DbBackend>>,
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
            reglages: None,
        }
    }

    /// Donne au client la base des réglages : sans elle, il refuse toute
    /// écoute de contact (`relay.circle_stream_request`).
    pub fn avec_reglages(mut self, backend: Arc<dyn DbBackend>) -> Self {
        self.reglages = Some(backend);
        self
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
                // APRÈS les en-têtes du distant : voir `ENTETE_RELAIS`.
                req = req.header(ENTETE_RELAIS, "1");
                if let Some(b) = body {
                    req = req.body(b);
                }

                let ws_tx = self.ws_tx.clone();
                let id_clone = id.clone();
                tokio::spawn(async move {
                    let resp = match req.send().await {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let hdrs = entetes_de_reponse_dapi(resp.headers());
                            let octets = resp.bytes().await.unwrap_or_default();
                            reponse_dapi(&id_clone, status, hdrs, &octets)
                        }
                        Err(e) => {
                            warn!(id = %id_clone, error = %e, "relay local dispatch failed");
                            let mut hdrs = serde_json::Map::new();
                            hdrs.insert(
                                "content-type".to_string(),
                                serde_json::Value::String("application/json".into()),
                            );
                            let corps = format!("{{\"error\": \"local dispatch failed: {e}\"}}");
                            reponse_dapi(&id_clone, 502, hdrs, corps.as_bytes())
                        }
                    };

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
                    pomper_le_flux(&http, &url, range, &id, &ws_tx).await;
                });
            }
            MESSAGE_FLUX_DE_CERCLE => {
                self.servir_un_flux_de_cercle(&v).await;
            }
            _ => {}
        }
    }
}

/// Message du pont : un contact écoute une piste de CE serveur (Tune Circle
/// T4, #5327). Voir [`RelayClient::servir_un_flux_de_cercle`].
pub const MESSAGE_FLUX_DE_CERCLE: &str = "relay.circle_stream_request";

/// Le `track_id` d'une écoute de contact, s'il désigne bien UNE piste : des
/// chiffres ASCII, un entier strictement positif, rien d'autre. Ni signe, ni
/// espace, ni `/`, ni `.`, ni encodage `%` : ce qui n'est pas un entier ne
/// devient jamais un morceau de chemin.
///
/// Le pont envoie une chaîne ; un nombre JSON est admis aussi.
pub(crate) fn track_id_de_cercle(valeur: Option<&serde_json::Value>) -> Option<i64> {
    let texte = match valeur? {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.as_u64()?.to_string(),
        _ => return None,
    };
    if texte.is_empty() || texte.len() > 19 || !texte.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    texte.parse::<i64>().ok().filter(|n| *n > 0)
}

/// Le greffon `circle` est-il installé et actif sur ce serveur ?
///
/// Même règle que le chargeur de greffons (`plugin_sdk::setup_all`) pour un
/// greffon opt-in : actif si `plugin_circle_installed` vaut `true` et que
/// `plugin_circle_enabled` ne vaut pas `false`. Relu à chaque demande : une
/// désinstallation vaut dès l'écoute suivante, même cloud injoignable.
pub(crate) fn greffon_circle_actif(reglages: &SettingsRepo) -> bool {
    let installe = reglages
        .get("plugin_circle_installed")
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true");
    let desactive = reglages
        .get("plugin_circle_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "false");
    installe && !desactive
}

impl RelayClient {
    /// `relay.circle_stream_request { id, track_id, range }` : servir l'audio
    /// d'UNE piste de la bibliothèque, et rien d'autre.
    ///
    /// * **Une seule route atteignable** : `/api/v1/library/tracks/{id}/audio`,
    ///   `id` réduit à un entier par [`track_id_de_cercle`]. Tout autre
    ///   `track_id` est refusé en 404 SANS appel local.
    /// * **Le droit n'est pas jugé ici.** Le cloud l'a jugé en délivrant le
    ///   billet, le pont l'a revérifié à cette requête, et le message arrive
    ///   par la connexion authentifiée au pont (le seul canal qui porte ce
    ///   message). Ce serveur ne garde rien du cercle.
    /// * **Le propriétaire garde la main** : greffon `circle` désinstallé ou
    ///   désactivé → 404, sans appel local.
    /// * **Même chemin de morceaux** que `relay.stream_request`, `Range`
    ///   compris : le fichier d'origine, octet pour octet (bit-perfect).
    /// * **Journal** : piste, statut, octets, durée. Ni identité, ni billet —
    ///   ce serveur ne les reçoit d'ailleurs pas.
    async fn servir_un_flux_de_cercle(&self, v: &serde_json::Value) {
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let range = v
            .get("range")
            .and_then(|r| r.as_str())
            .map(|s| s.to_string());

        let permis = self
            .reglages
            .as_ref()
            .is_some_and(|b| greffon_circle_actif(&SettingsRepo::with_backend(b.clone())));
        let track_id = track_id_de_cercle(v.get("track_id"));

        let (Some(track_id), true) = (track_id, permis) else {
            if permis {
                warn!("circle_ecoute_track_id_refuse");
            } else {
                info!("circle_ecoute_refusee_greffon_inactif");
            }
            let refus = serde_json::json!({
                "type": "relay.stream_start",
                "id": id,
                "status": 404,
                "headers": {},
            });
            emettre_vers_le_relais(&self.ws_tx, refus.to_string()).await;
            return;
        };

        let url = format!(
            "http://127.0.0.1:{}/api/v1/library/tracks/{}/audio",
            self.local_port, track_id
        );
        let ws_tx = self.ws_tx.clone();
        let http = self.http_client.clone();
        tokio::spawn(async move {
            let debut = std::time::Instant::now();
            let bilan = pomper_le_flux(&http, &url, range, &id, &ws_tx).await;
            info!(
                track_id,
                statut = bilan.statut,
                octets = bilan.octets,
                duree_ms = debut.elapsed().as_millis() as u64,
                "circle_ecoute_de_contact"
            );
        });
    }
}

/// Ce qu'un flux relayé a donné, pour le journal.
pub(crate) struct BilanDuFlux {
    pub statut: u16,
    pub octets: u64,
}

/// Lit `url` en local (avec le `Range` reçu) et le pousse au relais :
/// `relay.stream_start`, puis les morceaux `BINARY:<id>:<base64>`, puis
/// `relay.stream_end`. Chemin commun à `relay.stream_request` et à
/// `relay.circle_stream_request`.
pub(crate) async fn pomper_le_flux(
    http: &reqwest::Client,
    url: &str,
    range: Option<String>,
    id: &str,
    ws_tx: &Arc<tokio::sync::Mutex<Option<mpsc::Sender<String>>>>,
) -> BilanDuFlux {
    let mut req = http.get(url);
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

            emettre_vers_le_relais(ws_tx, start_msg.to_string()).await;

            let mut octets: u64 = 0;
            use futures_util::StreamExt;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        octets += bytes.len() as u64;
                        // Trame TEXTE `BINARY:<id>:<base64>`. Le canal vers le
                        // relais ne porte que du texte (`mpsc::Sender<String>`),
                        // d'ou l'encodage. Une trame binaire prefixee de
                        // l'identifiant etait assemblee ici puis jetee sans
                        // etre envoyee : elle laissait croire a un second
                        // format de fil qui n'a jamais existe.
                        //
                        // C'est ICI que la liaison lente se fait sentir : ce
                        // `send` attend que le relais ait de la place. Il
                        // attend sans le verrou, sinon le `relay.pong` ne
                        // partirait plus et la session entiere serait coupee.
                        emettre_vers_le_relais(
                            ws_tx,
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
            emettre_vers_le_relais(ws_tx, end_msg.to_string()).await;
            BilanDuFlux {
                statut: status,
                octets,
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
            emettre_vers_le_relais(ws_tx, resp.to_string()).await;
            BilanDuFlux {
                statut: 502,
                octets: 0,
            }
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

    type Emetteur = Arc<Mutex<Option<mpsc::Sender<String>>>>;

    fn canal(capacite: usize) -> (Emetteur, mpsc::Receiver<String>) {
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
/// En-têtes d'une réponse d'API rendus au navigateur distant.
///
/// Le type de contenu (repli `application/json`, comme avant), plus ce qui
/// change ce que le navigateur fait de la réponse : le nom du fichier d'un
/// export (`content-disposition`) et les validateurs de cache d'une pochette
/// (`cache-control`, `etag`, `last-modified`). Pas `content-length` : le pont
/// le recalcule sur le corps qu'il rend.
pub(crate) fn entetes_de_reponse_dapi(
    entetes: &reqwest::header::HeaderMap,
) -> serde_json::Map<String, serde_json::Value> {
    let mut sortie = serde_json::Map::new();
    let type_contenu = entetes
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json");
    sortie.insert(
        "content-type".to_string(),
        serde_json::Value::String(type_contenu.to_string()),
    );
    for nom in [
        "content-disposition",
        "cache-control",
        "etag",
        "last-modified",
    ] {
        if let Some(valeur) = entetes.get(nom).and_then(|v| v.to_str().ok()) {
            sortie.insert(
                nom.to_string(),
                serde_json::Value::String(valeur.to_string()),
            );
        }
    }
    sortie
}

/// La trame `relay.response` d'une réponse d'API.
///
/// Un corps valide en UTF-8 part dans `body`, en texte, comme toujours : un
/// pont plus ancien le lit sans changement, et l'aller-retour est exact. Tout
/// autre corps — une pochette JPEG, une archive — part en base64 dans
/// `body_base64` : passé par `text()`, chaque octet invalide devenait U+FFFD
/// et l'image arrivait corrompue.
pub(crate) fn reponse_dapi(
    id: &str,
    status: u16,
    entetes: serde_json::Map<String, serde_json::Value>,
    octets: &[u8],
) -> serde_json::Value {
    let mut trame = serde_json::json!({
        "type": "relay.response",
        "id": id,
        "status": status,
        "headers": entetes,
    });
    match std::str::from_utf8(octets) {
        Ok(texte) => trame["body"] = serde_json::Value::String(texte.to_string()),
        Err(_) => trame["body_base64"] = serde_json::Value::String(base64_encode(octets)),
    }
    trame
}

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
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
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

pub fn spawn_relay_client(
    backend: Arc<dyn DbBackend>,
    local_port: u16,
) -> Option<Arc<RelayClient>> {
    let settings = &SettingsRepo::with_backend(backend.clone());
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

    let client = Arc::new(
        RelayClient::new(server_id, bridge_token, relay_url, local_port).avec_reglages(backend),
    );
    client.clone().spawn();
    Some(client)
}

/// Tune Circle T4 (#5327) : `relay.circle_stream_request`, contre un faux
/// serveur local qui note chaque chemin demandé.
#[cfg(test)]
mod flux_de_cercle_tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use std::sync::Mutex as StdMutex;

    const PATIENCE: Duration = Duration::from_secs(5);

    /// (chemin, `Range`) de chaque requête reçue par le faux serveur local.
    type Journal = Arc<StdMutex<Vec<(String, Option<String>)>>>;

    /// Le faux serveur local : seule la piste 42 existe, et son audio répond
    /// à un `Range` comme la vraie route (#3579). Tout le reste : 404.
    async fn repondre(State(journal): State<Journal>, uri: Uri, headers: HeaderMap) -> Response {
        let range = headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        journal
            .lock()
            .unwrap()
            .push((uri.path().to_string(), range.clone()));
        if uri.path() != "/api/v1/library/tracks/42/audio" {
            return StatusCode::NOT_FOUND.into_response();
        }
        match range.as_deref() {
            Some("bytes=0-3") => (
                StatusCode::PARTIAL_CONTENT,
                [
                    ("content-type", "audio/flac"),
                    ("content-range", "bytes 0-3/8"),
                    ("accept-ranges", "bytes"),
                ],
                b"fLaC".to_vec(),
            )
                .into_response(),
            _ => (
                StatusCode::OK,
                [("content-type", "audio/flac"), ("accept-ranges", "bytes")],
                b"fLaC\0\0\0\x22".to_vec(),
            )
                .into_response(),
        }
    }

    async fn serveur_local() -> (u16, Journal) {
        let journal: Journal = Arc::new(StdMutex::new(Vec::new()));
        let app = Router::new().fallback(repondre).with_state(journal.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        (port, journal)
    }

    fn base(installe: Option<&str>, active: Option<&str>) -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let s = SettingsRepo::with_backend(backend.clone());
        if let Some(v) = installe {
            s.set("plugin_circle_installed", v).unwrap();
        }
        if let Some(v) = active {
            s.set("plugin_circle_enabled", v).unwrap();
        }
        backend
    }

    async fn client(
        port: u16,
        reglages: Option<Arc<dyn DbBackend>>,
    ) -> (RelayClient, mpsc::Receiver<String>) {
        let mut c = RelayClient::new("srv".into(), "jeton-du-pont".into(), "ws://x".into(), port);
        if let Some(b) = reglages {
            c = c.avec_reglages(b);
        }
        let (tx, rx) = mpsc::channel::<String>(64);
        *c.ws_tx.lock().await = Some(tx);
        (c, rx)
    }

    fn demande(track_id: serde_json::Value, range: Option<&str>) -> String {
        serde_json::json!({
            "type": MESSAGE_FLUX_DE_CERCLE,
            "id": "req-c",
            "track_id": track_id,
            "range": range,
        })
        .to_string()
    }

    async fn trame(rx: &mut mpsc::Receiver<String>) -> String {
        tokio::time::timeout(PATIENCE, rx.recv())
            .await
            .expect("aucune trame vers le pont")
            .expect("canal ferme")
    }

    fn json(t: &str) -> serde_json::Value {
        serde_json::from_str(t).unwrap()
    }

    /// Une piste existante : son audio, `Range` compris, par le chemin de
    /// morceaux de `relay.stream_request` — et seulement sa route.
    #[tokio::test]
    async fn sert_l_audio_d_une_piste_existante_avec_range() {
        let (port, journal) = serveur_local().await;
        let (c, mut rx) = client(port, Some(base(Some("true"), None))).await;

        c.handle_message(&demande(serde_json::json!("42"), Some("bytes=0-3")))
            .await;

        let debut = json(&trame(&mut rx).await);
        assert_eq!(debut["type"], "relay.stream_start");
        assert_eq!(debut["id"], "req-c");
        assert_eq!(debut["status"], 206);
        assert_eq!(debut["headers"]["content-range"], "bytes 0-3/8");
        assert_eq!(debut["headers"]["content-type"], "audio/flac");
        assert_eq!(trame(&mut rx).await, "BINARY:req-c:ZkxhQw==");
        assert_eq!(json(&trame(&mut rx).await)["type"], "relay.stream_end");

        assert_eq!(
            *journal.lock().unwrap(),
            vec![(
                "/api/v1/library/tracks/42/audio".to_string(),
                Some("bytes=0-3".to_string())
            )]
        );
    }

    /// Un `track_id` inconnu : le 404 de la route locale, relayé.
    #[tokio::test]
    async fn un_track_id_inconnu_rend_404() {
        let (port, journal) = serveur_local().await;
        let (c, mut rx) = client(port, Some(base(Some("true"), None))).await;

        c.handle_message(&demande(serde_json::json!(999), None))
            .await;

        let debut = json(&trame(&mut rx).await);
        assert_eq!(debut["status"], 404);
        assert_eq!(
            journal.lock().unwrap()[0].0,
            "/api/v1/library/tracks/999/audio"
        );
    }

    /// Aucune autre route n'est atteignable par ce message : tout ce qui n'est
    /// pas un entier positif est refusé en 404, SANS aucun appel local.
    #[tokio::test]
    async fn aucune_autre_route_n_est_atteignable() {
        let (port, journal) = serveur_local().await;
        let (c, mut rx) = client(port, Some(base(Some("true"), None))).await;

        for mauvais in [
            serde_json::json!("../1"),
            serde_json::json!("42/../../../system/health"),
            serde_json::json!("1/../../stream/1"),
            serde_json::json!("42/"),
            serde_json::json!("/42"),
            serde_json::json!("42?x=1"),
            serde_json::json!("4%32"),
            serde_json::json!(" 42"),
            serde_json::json!("-1"),
            serde_json::json!("0"),
            serde_json::json!(""),
            serde_json::json!("1.0"),
            serde_json::json!("99999999999999999999"),
            serde_json::json!(-5),
            serde_json::json!(1.5),
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(["42"]),
        ] {
            c.handle_message(&demande(mauvais.clone(), None)).await;
            let debut = json(&trame(&mut rx).await);
            assert_eq!(debut["status"], 404, "track_id {mauvais}");
            assert_eq!(debut["id"], "req-c");
        }
        // Sans `track_id` du tout.
        c.handle_message(
            &serde_json::json!({"type": MESSAGE_FLUX_DE_CERCLE, "id": "req-c"}).to_string(),
        )
        .await;
        assert_eq!(json(&trame(&mut rx).await)["status"], 404);

        assert!(
            journal.lock().unwrap().is_empty(),
            "un appel local est parti : {:?}",
            journal.lock().unwrap()
        );
    }

    /// Le propriétaire garde la main : greffon non installé, désactivé, ou
    /// client sans réglages → 404, aucun appel local.
    #[tokio::test]
    async fn greffon_circle_inactif_rien_n_est_servi() {
        let (port, journal) = serveur_local().await;
        for reglages in [
            None,
            Some(base(None, None)),
            Some(base(Some("false"), None)),
            Some(base(Some("true"), Some("false"))),
        ] {
            let (c, mut rx) = client(port, reglages).await;
            c.handle_message(&demande(serde_json::json!("42"), None))
                .await;
            assert_eq!(json(&trame(&mut rx).await)["status"], 404);
        }
        assert!(journal.lock().unwrap().is_empty());
    }

    /// Non-régression : `relay.stream_request` suit toujours `/stream/{id}`.
    #[tokio::test]
    async fn le_flux_d_orchestrateur_suit_toujours_son_chemin() {
        let (port, journal) = serveur_local().await;
        let (c, mut rx) = client(port, None).await;
        c.handle_message(
            &serde_json::json!({"type": "relay.stream_request", "id": "s", "stream_id": "abc"})
                .to_string(),
        )
        .await;
        assert_eq!(json(&trame(&mut rx).await)["status"], 404);
        assert_eq!(journal.lock().unwrap()[0].0, "/stream/abc");
    }
}

/// `relay.request` (l'API par le pont), contre un faux serveur local : ce que
/// le navigateur distant reçoit doit être ce que le serveur a rendu.
#[cfg(test)]
mod reponse_dapi_tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, Uri};
    use axum::response::{IntoResponse, Response};
    use std::sync::Mutex as StdMutex;

    const PATIENCE: Duration = Duration::from_secs(5);

    /// En-tête JPEG : invalide en UTF-8, comme toute vraie pochette.
    const JPEG: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0];

    /// (chemin + requête, `X-Tune-Profile`) de chaque appel reçu.
    type Journal = Arc<StdMutex<Vec<(String, Option<String>)>>>;

    async fn repondre(State(journal): State<Journal>, uri: Uri, headers: HeaderMap) -> Response {
        let profil = headers
            .get("x-tune-profile")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        journal.lock().unwrap().push((uri.to_string(), profil));
        match uri.path() {
            "/api/v1/library/artwork/a.jpg" => (
                [
                    ("content-type", "image/jpeg"),
                    ("cache-control", "public, max-age=86400"),
                    ("etag", "\"pochette-a\""),
                ],
                JPEG.to_vec(),
            )
                .into_response(),
            _ => (
                [
                    ("content-type", "text/csv; charset=utf-8"),
                    (
                        "content-disposition",
                        "attachment; filename=\"tune-history.csv\"",
                    ),
                ],
                "date;titre\n2026-10-09;Écoute\n",
            )
                .into_response(),
        }
    }

    async fn serveur_local() -> (u16, Journal) {
        let journal: Journal = Arc::new(StdMutex::new(Vec::new()));
        let app = Router::new().fallback(repondre).with_state(journal.clone());
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.unwrap();
        });
        (port, journal)
    }

    async fn reponse_a(port: u16, path: &str) -> serde_json::Value {
        let c = RelayClient::new("srv".into(), "jeton".into(), "ws://x".into(), port);
        let (tx, mut rx) = mpsc::channel::<String>(8);
        *c.ws_tx.lock().await = Some(tx);
        c.handle_message(
            &serde_json::json!({
                "type": "relay.request",
                "id": "req-a",
                "method": "GET",
                "path": path,
                "headers": {"x-tune-profile": "3"},
            })
            .to_string(),
        )
        .await;
        let trame = tokio::time::timeout(PATIENCE, rx.recv())
            .await
            .expect("aucune reponse vers le pont")
            .expect("canal ferme");
        serde_json::from_str(&trame).unwrap()
    }

    /// LE défaut : une pochette passait par `text()`, qui remplace chaque
    /// octet invalide en UTF-8 par U+FFFD. L'image arrivait corrompue.
    #[tokio::test]
    async fn une_pochette_binaire_arrive_intacte() {
        let (port, _) = serveur_local().await;
        let r = reponse_a(port, "/api/v1/library/artwork/a.jpg?size=300").await;
        assert_eq!(r["type"], "relay.response");
        assert_eq!(r["status"], 200);
        assert_eq!(r["headers"]["content-type"], "image/jpeg");
        assert_eq!(
            r["body_base64"], "/9j/4A==",
            "corps binaire non transmis en base64 : {r}"
        );
    }

    /// Les validateurs de cache reviennent avec la pochette : sans eux, le
    /// navigateur la redemande en entier à chaque écran.
    #[tokio::test]
    async fn les_entetes_de_cache_reviennent() {
        let (port, _) = serveur_local().await;
        let r = reponse_a(port, "/api/v1/library/artwork/a.jpg").await;
        assert_eq!(r["headers"]["cache-control"], "public, max-age=86400");
        assert_eq!(r["headers"]["etag"], "\"pochette-a\"");
    }

    /// Un export garde son nom de fichier, et un corps texte reste du texte
    /// lisible par un pont plus ancien.
    #[tokio::test]
    async fn un_export_garde_son_nom_et_son_texte() {
        let (port, _) = serveur_local().await;
        let r = reponse_a(port, "/api/v1/history/export?limit=10000").await;
        assert_eq!(
            r["headers"]["content-disposition"],
            "attachment; filename=\"tune-history.csv\""
        );
        assert_eq!(r["body"], "date;titre\n2026-10-09;Écoute\n");
        assert!(r.get("body_base64").is_none());
    }

    /// La requête et le profil arrivent jusqu'à la route locale.
    #[tokio::test]
    async fn la_requete_et_le_profil_atteignent_la_route() {
        let (port, journal) = serveur_local().await;
        reponse_a(port, "/api/v1/library/artwork/a.jpg?size=300").await;
        assert_eq!(
            *journal.lock().unwrap(),
            vec![(
                "/api/v1/library/artwork/a.jpg?size=300".to_string(),
                Some("3".to_string())
            )]
        );
    }
}
