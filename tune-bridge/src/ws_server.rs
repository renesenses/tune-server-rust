use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol;
use crate::state::{CorpsRelaye, RelayState};

/// Prefixe des trames portant un morceau audio.
///
/// Le serveur les envoie en trame **texte**, `BINARY:<id>:<base64>`, et non en
/// trame binaire : c'est le format reellement emis par
/// `tune-core/src/cloud/relay.rs`. Le nom est trompeur, il est ici pour ce
/// qu'il est — le contrat de fil deja deploye chez les utilisateurs.
const PREFIXE_MORCEAU: &str = "BINARY:";

/// Morceaux en attente avant que le relais ne freine le serveur.
///
/// Le canal borne fait le contre-pression : si le navigateur lit moins vite
/// que le disque ne debite, l'envoi cote serveur attend au lieu de remplir la
/// memoire du relais avec un album entier.
const MORCEAUX_EN_ATTENTE: usize = 64;

pub async fn handle_server_ws(socket: WebSocket, state: Arc<RelayState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for relay.register
    let register: protocol::RelayRegister = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(text))) => {
                let msg_type = protocol::parse_message_type(&text);
                if msg_type.as_deref() == Some("relay.register") {
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(v) => match serde_json::from_value::<protocol::RelayRegister>(v) {
                            Ok(r) => break r,
                            Err(e) => {
                                warn!(error = %e, "invalid relay.register payload");
                                continue;
                            }
                        },
                        Err(_) => continue,
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = ws_tx.send(Message::Pong(data)).await;
            }
            None | Some(Err(_)) => return,
            _ => continue,
        }
    };

    info!(
        server_id = %register.server_id,
        server_name = %register.server_name,
        version = %register.version,
        "server registering"
    );

    // Le Cloud Relay est PREMIUM. Le controle vivait cote serveur, sur
    // `POST /cloud/bridge/enable` — la porte que l'utilisateur tient. Ici, on
    // demande au cloud ce qu'il en est avant d'ouvrir la notre.
    if let crate::licence::Verdict::Refuse(motif) =
        state.licences.verifier(&register.server_id).await
    {
        warn!(server_id = %register.server_id, motif, "register refuse — licence");
        // Le motif voyage jusqu'au serveur : il pourra le dire a son
        // utilisateur au lieu de se reconnecter en boucle sans comprendre.
        let reject = serde_json::json!({
            "type": "relay.registered",
            "ok": false,
            "error": motif,
        });
        let _ = ws_tx.send(Message::Text(reject.to_string().into())).await;
        return;
    }

    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(256);

    if let Err(reason) = state.register_server(
        register.server_id.clone(),
        register.server_name.clone(),
        register.bridge_token,
        msg_tx,
    ) {
        // Log the server_id and reason, never the bridge_token.
        warn!(server_id = %register.server_id, reason, "register rejected");
        let reject = serde_json::json!({
            "type": "relay.registered",
            "ok": false,
            "error": reason
        });
        let _ = ws_tx.send(Message::Text(reject.to_string().into())).await;
        return;
    }

    let ack = protocol::RelayRegistered {
        msg_type: "relay.registered",
        ok: true,
        server_id: register.server_id.clone(),
    };
    let _ = ws_tx
        .send(Message::Text(serde_json::to_string(&ack).unwrap().into()))
        .await;

    info!(server_id = %register.server_id, "server registered");

    let server_id = register.server_id.clone();
    let server_id_writer = server_id.clone();
    let state_writer = state.clone();

    // Writer task: relay → server WS
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if ws_tx.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
        // If writer dies, unregister
        state_writer.unregister_server(&server_id_writer);
    });

    // Reader loop: server WS → relay
    let heartbeat_timeout = tokio::time::Duration::from_secs(90);
    let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
    heartbeat_interval.tick().await; // skip first immediate tick

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_server_message(&state, &server_id, &text).await;
                    }
                    Some(Ok(Message::Binary(data))) => {
                        handle_server_binary(&state, &server_id, &data).await;
                    }
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                if let Some(conn) = state.servers.get(&server_id) {
                    if conn.last_heartbeat.elapsed() > heartbeat_timeout {
                        warn!(server_id = %server_id, "heartbeat timeout");
                        break;
                    }
                }
            }
        }
    }

    // Cleanup
    writer_handle.abort();
    state.unregister_server(&server_id);
    info!(server_id = %server_id, "server disconnected");
}

async fn handle_server_message(state: &RelayState, server_id: &str, text: &str) {
    // Les morceaux audio passent AVANT l'analyse JSON : `BINARY:<id>:<base64>`
    // n'est pas du JSON, donc `parse_message_type` rend `None` et la trame
    // etait jetee sans un mot. C'est par la que tout le son disparaissait.
    if let Some(reste) = text.strip_prefix(PREFIXE_MORCEAU) {
        transmettre_morceau(state, server_id, reste).await;
        return;
    }

    let msg_type = match protocol::parse_message_type(text) {
        Some(t) => t,
        None => return,
    };

    match msg_type.as_str() {
        "relay.pong" => {
            if let Some(mut conn) = state.servers.get_mut(server_id) {
                conn.last_heartbeat = Instant::now();
            }
        }
        "relay.response" => {
            if let Ok(resp) = serde_json::from_str::<protocol::RelayResponse>(text) {
                resolve_pending(state, server_id, resp).await;
            }
        }
        "relay.event" => {
            // TODO Phase 1: forward to connected clients
        }
        "relay.stream_start" => {
            if let Ok(debut) = serde_json::from_str::<protocol::RelayStreamStart>(text) {
                ouvrir_flux(state, server_id, debut).await;
            } else {
                warn!(server_id = %server_id, "relay.stream_start illisible");
            }
        }
        "relay.stream_end" => {
            if let Ok(fin) = serde_json::from_str::<protocol::RelayStreamEnd>(text) {
                fermer_flux(state, server_id, &fin.id).await;
            }
        }
        _ => {
            warn!(server_id = %server_id, msg_type = %msg_type, "unknown server message");
        }
    }
}

/// Trames binaires du serveur.
///
/// Aucune n'est emise a ce jour : les morceaux audio voyagent en trames texte
/// prefixees `BINARY:` (voir `PREFIXE_MORCEAU`). L'arm existe pour ne pas
/// fermer la connexion sur une trame inattendue.
async fn handle_server_binary(_state: &RelayState, _server_id: &str, _data: &[u8]) {}

/// Debut d'un flux : on repond au navigateur, et on ouvre le puits.
///
/// L'en-tete part tout de suite — c'est ce qui permet au lecteur de commencer
/// a jouer avant la fin du fichier. Le corps suit, morceau par morceau.
async fn ouvrir_flux(state: &RelayState, server_id: &str, debut: protocol::RelayStreamStart) {
    let (pending, flux) = match state.servers.get(server_id) {
        // Les deux `Arc` sont clones puis le garde de la DashMap est relache :
        // garder un garde a travers un `await` sur la meme carte est le chemin
        // court vers l'interblocage.
        Some(conn) => (conn.pending.clone(), conn.flux.clone()),
        None => return,
    };

    let attendu = match pending.lock().await.remove(&debut.id) {
        Some(tx) => tx,
        // Personne n'attend : requete abandonnee ou deja expiree.
        None => return,
    };

    let mut headers = debut.headers;
    // `content_length` voyage dans son propre champ ; le navigateur, lui, le
    // lit dans l'en-tete. Sans lui, pas de duree ni de barre de progression.
    if let Some(taille) = debut.content_length {
        headers
            .entry("content-length".to_string())
            .or_insert_with(|| serde_json::Value::String(taille.to_string()));
    }

    // Un echec amont (502, 404, 416...) n'aura jamais de morceau ni de
    // `relay.stream_end` : lui ouvrir un puits laisserait le corps HTTP
    // pendre jusqu'a la deconnexion. On repond court, tout de suite.
    if debut.status >= 400 {
        let _ = attendu.send(crate::state::PendingResponse {
            status: debut.status,
            headers,
            body: CorpsRelaye::Entier(None),
        });
        return;
    }

    let (envoi, reception) = mpsc::channel::<Vec<u8>>(MORCEAUX_EN_ATTENTE);
    flux.lock().await.insert(debut.id.clone(), envoi);

    if attendu
        .send(crate::state::PendingResponse {
            status: debut.status,
            headers,
            body: CorpsRelaye::Morceaux(reception),
        })
        .is_err()
    {
        // Le client est parti entre-temps : pas de puits orphelin.
        flux.lock().await.remove(&debut.id);
    }
}

/// Un morceau audio, du serveur vers le corps HTTP qui l'attend.
async fn transmettre_morceau(state: &RelayState, server_id: &str, reste: &str) {
    let Some((id, encode)) = reste.split_once(':') else {
        warn!(server_id = %server_id, "morceau sans identifiant");
        return;
    };
    let Some(octets) = decoder_base64(encode) else {
        // Un morceau illisible n'est pas transmis : mieux vaut un trou franc
        // qu'un octet invente au milieu du son.
        warn!(server_id = %server_id, id, "morceau base64 illisible");
        return;
    };

    let flux = match state.servers.get(server_id) {
        Some(conn) => conn.flux.clone(),
        None => return,
    };

    // Le `Sender` est clone hors du verrou : `send` attend quand le canal est
    // plein (contre-pression voulue), et attendre le verrou en main bloquerait
    // tous les autres flux du meme serveur.
    let envoi = match flux.lock().await.get(id) {
        Some(tx) => tx.clone(),
        None => return,
    };

    if envoi.send(octets).await.is_err() {
        // Le navigateur a ferme l'onglet : le puits ne sert plus a rien.
        flux.lock().await.remove(id);
    }
}

/// Fin du flux : fermer le puits termine le corps HTTP.
async fn fermer_flux(state: &RelayState, server_id: &str, id: &str) {
    let flux = match state.servers.get(server_id) {
        Some(conn) => conn.flux.clone(),
        None => return,
    };
    flux.lock().await.remove(id);
}

async fn resolve_pending(state: &RelayState, server_id: &str, resp: protocol::RelayResponse) {
    let pending = match state.servers.get(server_id) {
        Some(conn) => conn.pending.clone(),
        None => return,
    };
    let attendu = pending.lock().await.remove(&resp.id);
    if let Some(tx) = attendu {
        let _ = tx.send(crate::state::PendingResponse {
            status: resp.status,
            headers: resp.headers,
            body: CorpsRelaye::Entier(resp.body),
        });
    }
}

/// Decodeur base64 strict, miroir de l'encodeur de `tune-core`.
///
/// Strict a dessein : longueur multiple de quatre, remplissage seulement dans
/// le dernier bloc, aucun caractere hors alphabet. Un decodeur tolerant
/// rendrait des octets plausibles pour une trame corrompue, et le defaut
/// s'entendrait a la place de se voir.
fn decoder_base64(texte: &str) -> Option<Vec<u8>> {
    let octets = texte.as_bytes();
    if octets.len() % 4 != 0 {
        return None;
    }
    let dernier = octets.len() / 4;
    let mut sortie = Vec::with_capacity(dernier * 3);

    for (rang, bloc) in octets.chunks(4).enumerate() {
        let mut assemble: u32 = 0;
        let mut utiles = 3usize;
        for (i, &c) in bloc.iter().enumerate() {
            if c == b'=' {
                // Le remplissage ne peut etre qu'en 3e ou 4e position, et
                // seulement dans le dernier bloc.
                if i < 2 || rang + 1 != dernier || bloc[i..].iter().any(|&d| d != b'=') {
                    return None;
                }
                utiles = i - 1;
                break;
            }
            assemble |= u32::from(valeur_base64(c)?) << (18 - 6 * i);
        }
        sortie.push((assemble >> 16) as u8);
        if utiles > 1 {
            sortie.push((assemble >> 8) as u8);
        }
        if utiles > 2 {
            sortie.push(assemble as u8);
        }
    }
    Some(sortie)
}

fn valeur_base64(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod relais_de_flux_tests {
    use super::*;
    use crate::state::PendingResponse;
    use tokio::sync::oneshot;

    fn relais() -> RelayState {
        RelayState::new()
    }

    async fn serveur_enregistre(state: &RelayState) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>(16);
        state
            .register_server("srv".into(), "Salon".into(), "tok".into(), tx)
            .unwrap();
        rx
    }

    /// Depose l'attente qu'aurait posee `proxy_stream` juste avant d'emettre
    /// sa `relay.stream_request`.
    async fn attente_deposee(state: &RelayState, id: &str) -> oneshot::Receiver<PendingResponse> {
        let (tx, rx) = oneshot::channel::<PendingResponse>();
        let pending = state.servers.get("srv").unwrap().pending.clone();
        pending.lock().await.insert(id.to_string(), tx);
        rx
    }

    /// Toute attente de ce module est bornee : un correctif degrade doit
    /// RATER, pas suspendre la suite de tests indefiniment.
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

    async fn reponse_attendue(attente: oneshot::Receiver<PendingResponse>) -> PendingResponse {
        tokio::time::timeout(PATIENCE, attente)
            .await
            .expect("le relais n'a jamais repondu : la requete ira jusqu'au 504")
            .expect("l'attente a ete abandonnee sans reponse")
    }

    async fn morceau_attendu(morceaux: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        tokio::time::timeout(PATIENCE, morceaux.recv())
            .await
            .expect("aucun morceau n'est arrive : le corps HTTP reste muet")
    }

    fn debut(id: &str, status: u16, taille: Option<u64>) -> String {
        serde_json::json!({
            "type": "relay.stream_start",
            "id": id,
            "status": status,
            "headers": { "content-type": "audio/flac" },
            "content_length": taille,
        })
        .to_string()
    }

    /// LE defaut. Les morceaux audio voyagent en trames texte
    /// `BINARY:<id>:<base64>`, qui ne sont pas du JSON : le relais les jetait
    /// sans un mot, ne resolvait jamais l'attente, et toute requete de flux
    /// distante finissait en 504 au bout de trente secondes. Pas un octet de
    /// son n'a jamais traverse le pont.
    #[tokio::test]
    async fn le_flux_repond_tout_de_suite_puis_les_morceaux_suivent() {
        let state = relais();
        let _serveur = serveur_enregistre(&state).await;
        let attente = attente_deposee(&state, "req-1").await;

        handle_server_message(&state, "srv", &debut("req-1", 200, Some(9))).await;

        let reponse = reponse_attendue(attente).await;
        assert_eq!(reponse.status, 200);
        assert_eq!(reponse.headers["content-type"], "audio/flac");
        // `content_length` voyage dans son champ ; sans report en en-tete, le
        // navigateur n'a ni duree ni barre de progression.
        assert_eq!(reponse.headers["content-length"], "9");
        let mut morceaux = match reponse.body {
            CorpsRelaye::Morceaux(rx) => rx,
            CorpsRelaye::Entier(_) => panic!("un flux audio n'est pas un corps entier"),
        };

        // "fLaC" puis "\0\0\0\x22" — deux trames, dans cet ordre.
        handle_server_message(&state, "srv", "BINARY:req-1:ZkxhQw==").await;
        handle_server_message(&state, "srv", "BINARY:req-1:AAAAIg==").await;
        assert_eq!(
            morceau_attendu(&mut morceaux).await.unwrap(),
            b"fLaC".to_vec()
        );
        assert_eq!(
            morceau_attendu(&mut morceaux).await.unwrap(),
            vec![0, 0, 0, 0x22]
        );

        handle_server_message(
            &state,
            "srv",
            &serde_json::json!({"type": "relay.stream_end", "id": "req-1"}).to_string(),
        )
        .await;
        assert_eq!(
            morceau_attendu(&mut morceaux).await,
            None,
            "la fin doit clore le corps"
        );
    }

    /// Un morceau adresse a un flux inconnu ne doit reveiller personne : c'est
    /// le cas d'une requete deja abandonnee.
    #[tokio::test]
    async fn un_morceau_sans_flux_ouvert_est_ignore() {
        let state = relais();
        let _serveur = serveur_enregistre(&state).await;
        handle_server_message(&state, "srv", "BINARY:inconnu:ZkxhQw==").await;
        assert!(
            state
                .servers
                .get("srv")
                .unwrap()
                .flux
                .lock()
                .await
                .is_empty()
        );
    }

    /// Un echec amont n'enverra ni morceau ni `relay.stream_end`. Lui ouvrir
    /// un puits laisserait le corps HTTP ouvert jusqu'a la deconnexion.
    #[tokio::test]
    async fn un_echec_amont_repond_court_sans_ouvrir_de_puits() {
        let state = relais();
        let _serveur = serveur_enregistre(&state).await;
        let attente = attente_deposee(&state, "req-2").await;

        handle_server_message(&state, "srv", &debut("req-2", 502, None)).await;

        let reponse = reponse_attendue(attente).await;
        assert_eq!(reponse.status, 502);
        assert!(matches!(reponse.body, CorpsRelaye::Entier(None)));
        assert!(
            state
                .servers
                .get("srv")
                .unwrap()
                .flux
                .lock()
                .await
                .is_empty()
        );
    }

    /// Le serveur tombe au milieu d'un morceau : le corps HTTP doit se fermer,
    /// pas rester ouvert a attendre une suite qui ne viendra jamais.
    #[tokio::test]
    async fn la_deconnexion_du_serveur_ferme_les_corps_ouverts() {
        let state = relais();
        let _serveur = serveur_enregistre(&state).await;
        let attente = attente_deposee(&state, "req-3").await;
        handle_server_message(&state, "srv", &debut("req-3", 200, None)).await;
        let mut morceaux = match reponse_attendue(attente).await.body {
            CorpsRelaye::Morceaux(rx) => rx,
            CorpsRelaye::Entier(_) => panic!("un flux audio n'est pas un corps entier"),
        };

        state.unregister_server("srv");
        assert_eq!(morceau_attendu(&mut morceaux).await, None);
    }

    /// Une reponse d'API reste ce qu'elle etait : un corps entier.
    #[tokio::test]
    async fn une_reponse_dapi_reste_un_corps_entier() {
        let state = relais();
        let _serveur = serveur_enregistre(&state).await;
        let attente = attente_deposee(&state, "req-4").await;

        handle_server_message(
            &state,
            "srv",
            &serde_json::json!({
                "type": "relay.response",
                "id": "req-4",
                "status": 200,
                "headers": {"content-type": "application/json"},
                "body": "{\"ok\":true}",
            })
            .to_string(),
        )
        .await;

        let reponse = reponse_attendue(attente).await;
        match reponse.body {
            CorpsRelaye::Entier(Some(corps)) => assert_eq!(corps, "{\"ok\":true}"),
            _ => panic!("une reponse d'API n'est pas un flux"),
        }
    }

    /// Le corps HTTP rend les morceaux dans l'ordre, et se termine avec eux.
    #[tokio::test]
    async fn le_corps_http_rend_les_morceaux_dans_lordre() {
        let (envoi, reception) = mpsc::channel::<Vec<u8>>(4);
        envoi.send(b"fLaC".to_vec()).await.unwrap();
        envoi.send(vec![0, 0, 0, 0x22]).await.unwrap();
        drop(envoi);

        let reponse = crate::stream_proxy::reponse_relayee(PendingResponse {
            status: 206,
            headers: serde_json::Map::new(),
            body: CorpsRelaye::Morceaux(reception),
        });
        assert_eq!(reponse.status(), 206);

        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(octets.as_ref(), &[b'f', b'L', b'a', b'C', 0, 0, 0, 0x22]);
    }

    /// Miroir de l'encodeur de `tune-core` : les trois longueurs de bloc, dont
    /// les deux qui portent du remplissage.
    #[test]
    fn le_decodeur_base64_rend_ce_que_lencodeur_a_pris() {
        assert_eq!(decoder_base64("").unwrap(), Vec::<u8>::new());
        assert_eq!(decoder_base64("ZkxhQw==").unwrap(), b"fLaC".to_vec());
        assert_eq!(decoder_base64("YQ==").unwrap(), b"a".to_vec());
        assert_eq!(decoder_base64("YWI=").unwrap(), b"ab".to_vec());
        assert_eq!(decoder_base64("YWJj").unwrap(), b"abc".to_vec());
        assert_eq!(decoder_base64("//8A").unwrap(), vec![0xFF, 0xFF, 0x00]);
    }

    /// Strict a dessein : un decodeur tolerant rendrait des octets plausibles
    /// pour une trame corrompue, et le defaut s'entendrait au lieu de se voir.
    #[test]
    fn le_decodeur_base64_refuse_ce_qui_nen_est_pas() {
        assert_eq!(decoder_base64("ZkxhQ"), None, "longueur non multiple de 4");
        assert_eq!(decoder_base64("Zkxh*w=="), None, "caractere hors alphabet");
        assert_eq!(decoder_base64("=AAA"), None, "remplissage en tete");
        assert_eq!(
            decoder_base64("YQ==YWJj"),
            None,
            "remplissage hors du dernier bloc"
        );
        assert_eq!(decoder_base64("YQ=A"), None, "octet apres le remplissage");
    }
}
