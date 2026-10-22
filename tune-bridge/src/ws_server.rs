use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol;
use crate::state::{CorpsRelaye, Puits, RelayState};
use crate::trafic::{Categorie, Erreur};

/// Prefixe des trames portant un morceau audio.
///
/// Un serveur rc4 a rc6 envoie ses morceaux en trame **texte**,
/// `BINARY:<id>:<base64>` ; un serveur recent aussi, face a un pont qui n'a
/// pas annonce les trames binaires (#6564). Le nom est trompeur, il est ici
/// pour ce qu'il est — le contrat de fil deja deploye chez les utilisateurs.
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

    // Clé du serveur (#6603) : s'il en annonce une, il signe un défi. Si le
    // site connaît une clé pour ce server_id, rien d'autre n'est accepté.
    let cle_serveur = {
        let tx = &mut ws_tx;
        let rx = &mut ws_rx;
        let echange = |defi: String| async move {
            let message = serde_json::json!({"type": "relay.challenge", "defi": defi});
            tx.send(Message::Text(message.to_string().into()))
                .await
                .ok()?;
            tokio::time::timeout(DELAI_DEFI_SERVEUR, async {
                loop {
                    match rx.next().await {
                        Some(Ok(Message::Text(text))) => {
                            if protocol::parse_message_type(&text).as_deref()
                                == Some("relay.challenge_response")
                            {
                                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                                return v["signature"].as_str().map(str::to_string);
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            let _ = tx.send(Message::Pong(data)).await;
                        }
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return None,
                        _ => {}
                    }
                }
            })
            .await
            .ok()
            .flatten()
        };
        authentifier_serveur(
            &state,
            &register.server_id,
            register.server_pk.as_deref(),
            echange,
        )
        .await
    };
    let cle_serveur = match cle_serveur {
        Ok(c) => c,
        Err(reason) => {
            warn!(server_id = %register.server_id, reason, "register refuse — cle du serveur");
            let reject = serde_json::json!({
                "type": "relay.registered",
                "ok": false,
                "error": reason,
            });
            let _ = ws_tx.send(Message::Text(reject.to_string().into())).await;
            return;
        }
    };

    let (msg_tx, mut msg_rx) = mpsc::channel::<String>(256);

    if let Err(reason) = state.register_server(
        register.server_id.clone(),
        register.server_name.clone(),
        register.bridge_token,
        cle_serveur,
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
        jeton_herite: state.sessions.jeton_herite_ouvert(),
        laissez_passer: state.sessions.actif(),
        capabilities: protocol::CAPACITES,
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
    // Rangs attendus des morceaux binaires de cette connexion (#6564).
    let mut rangs = crate::morceau_binaire::Rangs::default();
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
                        handle_server_binary(&state, &server_id, &data, &mut rangs).await;
                    }
                    Some(Ok(Message::Ping(_))) => {}
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = heartbeat_interval.tick() => {
                if let Some(conn) = state.servers.get(&server_id)
                    && conn.last_heartbeat.elapsed() > heartbeat_timeout {
                        warn!(server_id = %server_id, "heartbeat timeout");
                        break;
                    }
            }
        }
    }

    // Cleanup
    writer_handle.abort();
    state.unregister_server(&server_id);
    info!(server_id = %server_id, "server disconnected");
}

/// Délai laissé au serveur pour signer le défi d'enregistrement.
const DELAI_DEFI_SERVEUR: std::time::Duration = std::time::Duration::from_secs(10);

/// Authentifie un serveur qui s'enregistre (#6603).
///
/// - il annonce une clé : `echanger` lui envoie un défi et rend sa
///   signature, vérifiée avec cette clé ;
/// - le site connaît une clé pour ce `server_id` : seul un serveur qui la
///   prouve est accepté. C'est ce qui empêche un tiers d'occuper le
///   `server_id` d'un serveur relié à un compte ;
/// - ni l'un ni l'autre : serveur antérieur, accepté comme avant.
///
/// Rend la clé prouvée, s'il y en a une.
pub(crate) async fn authentifier_serveur<F, Fut>(
    state: &RelayState,
    server_id: &str,
    server_pk: Option<&str>,
    echanger: F,
) -> Result<Option<String>, &'static str>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let attendue = state.sessions.cle_du_serveur(server_id).await;
    let prouvee = match server_pk.map(str::trim).filter(|k| !k.is_empty()) {
        Some(pk) => {
            let defi = tune_laissez_passer::jeton_aleatoire().ok_or("aleatoire indisponible")?;
            let signature = echanger(defi.clone())
                .await
                .ok_or("defi du serveur sans reponse")?;
            tune_laissez_passer::verifier_defi_serveur(pk, server_id, &defi, &signature)
                .map_err(|_| "signature du serveur invalide")?;
            Some(pk.to_string())
        }
        None => None,
    };
    if let Some(attendue) = attendue
        && prouvee.as_deref() != Some(attendue.as_str())
    {
        return Err("cle du serveur requise");
    }
    Ok(prouvee)
}

pub(crate) async fn handle_server_message(state: &RelayState, server_id: &str, text: &str) {
    // Les morceaux audio passent AVANT l'analyse JSON : `BINARY:<id>:<base64>`
    // n'est pas du JSON, donc `parse_message_type` rend `None` et la trame
    // etait jetee sans un mot. C'est par la que tout le son disparaissait.
    if let Some(reste) = text.strip_prefix(PREFIXE_MORCEAU) {
        transmettre_morceau(state, server_id, reste, text.len()).await;
        return;
    }

    let msg_type = match protocol::parse_message_type(text) {
        Some(t) => t,
        None => {
            compter_entree(state, server_id, Categorie::Api, text.len());
            return;
        }
    };

    match msg_type.as_str() {
        "relay.pong" => {
            compter_entree(state, server_id, Categorie::Api, text.len());
            if let Some(mut conn) = state.servers.get_mut(server_id) {
                conn.last_heartbeat = Instant::now();
            }
        }
        "relay.response" => {
            if let Ok(resp) = serde_json::from_str::<protocol::RelayResponse>(text) {
                resolve_pending(state, server_id, resp, text.len()).await;
            } else {
                compter_entree(state, server_id, Categorie::Api, text.len());
            }
        }
        "relay.event" => {
            compter_entree(state, server_id, Categorie::Api, text.len());
            // TODO Phase 1: forward to connected clients
        }
        "relay.stream_start" => {
            if let Ok(debut) = serde_json::from_str::<protocol::RelayStreamStart>(text) {
                ouvrir_flux(state, server_id, debut, text.len()).await;
            } else {
                compter_entree(state, server_id, Categorie::Api, text.len());
                warn!(server_id = %server_id, "relay.stream_start illisible");
            }
        }
        "relay.stream_end" => {
            if let Ok(fin) = serde_json::from_str::<protocol::RelayStreamEnd>(text) {
                fermer_flux(state, server_id, &fin.id, text.len()).await;
            } else {
                compter_entree(state, server_id, Categorie::Api, text.len());
            }
        }
        _ => {
            compter_entree(state, server_id, Categorie::Api, text.len());
            warn!(server_id = %server_id, msg_type = %msg_type, "unknown server message");
        }
    }
}

/// Trames binaires du serveur : les morceaux d'un flux (#6564), au format de
/// `crate::morceau_binaire`. Un serveur recent ne les envoie que si ce pont a
/// annonce la capacite ; un serveur anterieur envoie toujours le texte
/// `BINARY:` (voir `PREFIXE_MORCEAU`), et les deux chemins menent au meme
/// puits.
///
/// Un morceau perdu ou hors d'ordre coupe le flux : un corps HTTP tronque se
/// voit, un son recolle de travers s'entend.
///
/// La trame est comptee entiere (`data.len()`, en-tete compris) des qu'elle
/// est entree sur le VPS, qu'elle serve ou non (#6512) : dans la categorie de
/// son puits, `Audio` sans puits, `Api` si elle est illisible.
pub(crate) async fn handle_server_binary(
    state: &RelayState,
    server_id: &str,
    data: &[u8],
    rangs: &mut crate::morceau_binaire::Rangs,
) {
    let (flux, compteurs) = match state.servers.get(server_id) {
        Some(conn) => (conn.flux.clone(), conn.trafic.clone()),
        None => return,
    };

    let Some(morceau) = crate::morceau_binaire::lire(data) else {
        state.trafic.entree(&compteurs, Categorie::Api, data.len());
        warn!(server_id = %server_id, taille = data.len(), "morceau binaire illisible");
        return;
    };

    // Le `Sender` est clone hors du verrou, comme pour le texte.
    let puits = {
        let puits = flux.lock().await;
        if morceau.seq == 0 {
            // Un flux commence : on oublie les rangs des flux deja fermes.
            rangs.ne_garder_que(|id| puits.contains_key(id));
        }
        puits
            .get(morceau.id)
            .map(|p| (p.envoi.clone(), p.categorie))
    };
    let categorie = puits.as_ref().map_or(Categorie::Audio, |(_, c)| *c);
    state.trafic.entree(&compteurs, categorie, data.len());
    let Some((envoi, _)) = puits else {
        // Requete abandonnee : le flux n'a plus de puits.
        rangs.oublier(morceau.id);
        return;
    };

    if !rangs.accepter(morceau.id, morceau.seq) {
        warn!(
            server_id = %server_id,
            id = morceau.id,
            seq = morceau.seq,
            "morceau perdu ou hors d'ordre : flux coupe"
        );
        flux.lock().await.remove(morceau.id);
        return;
    }

    if envoi.send(morceau.octets.to_vec()).await.is_err() {
        // Le navigateur a ferme l'onglet : le puits ne sert plus a rien.
        flux.lock().await.remove(morceau.id);
    }
}

/// Octets entres sur le VPS, depuis ce serveur (#6512).
fn compter_entree(state: &RelayState, server_id: &str, categorie: Categorie, n: usize) {
    let compteurs = match state.servers.get(server_id) {
        Some(conn) => conn.trafic.clone(),
        None => return,
    };
    state.trafic.entree(&compteurs, categorie, n);
}

/// Debut d'un flux : on repond au navigateur, et on ouvre le puits.
///
/// L'en-tete part tout de suite — c'est ce qui permet au lecteur de commencer
/// a jouer avant la fin du fichier. Le corps suit, morceau par morceau.
async fn ouvrir_flux(
    state: &RelayState,
    server_id: &str,
    debut: protocol::RelayStreamStart,
    taille: usize,
) {
    let (pending, flux, compteurs) = match state.servers.get(server_id) {
        // Les `Arc` sont clones puis le garde de la DashMap est relache :
        // garder un garde a travers un `await` sur la meme carte est le chemin
        // court vers l'interblocage.
        Some(conn) => (conn.pending.clone(), conn.flux.clone(), conn.trafic.clone()),
        None => return,
    };

    let (attendu, categorie) = match pending.lock().await.remove(&debut.id) {
        Some(a) => (a.tx, a.categorie),
        // Personne n'attend : requete abandonnee ou deja expiree.
        None => {
            state.trafic.entree(&compteurs, Categorie::Audio, taille);
            return;
        }
    };
    state.trafic.entree(&compteurs, categorie, taille);

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
        if debut.status >= 500 {
            state.trafic.erreur(Erreur::Amont5xx);
        }
        let _ = attendu.send(crate::state::PendingResponse {
            status: debut.status,
            headers,
            body: CorpsRelaye::Entier(None),
        });
        return;
    }

    let (envoi, reception) = mpsc::channel::<Vec<u8>>(MORCEAUX_EN_ATTENTE);
    let fin_propre = Arc::new(std::sync::atomic::AtomicBool::new(false));
    flux.lock().await.insert(
        debut.id.clone(),
        Puits {
            envoi,
            categorie,
            fin_propre: fin_propre.clone(),
        },
    );

    if attendu
        .send(crate::state::PendingResponse {
            status: debut.status,
            headers,
            body: CorpsRelaye::Morceaux(reception, fin_propre),
        })
        .is_err()
    {
        // Le client est parti entre-temps : pas de puits orphelin.
        flux.lock().await.remove(&debut.id);
    }
}

/// Un morceau audio, du serveur vers le corps HTTP qui l'attend.
async fn transmettre_morceau(state: &RelayState, server_id: &str, reste: &str, taille: usize) {
    let (flux, compteurs) = match state.servers.get(server_id) {
        Some(conn) => (conn.flux.clone(), conn.trafic.clone()),
        None => return,
    };
    let Some((id, encode)) = reste.split_once(':') else {
        state.trafic.entree(&compteurs, Categorie::Audio, taille);
        warn!(server_id = %server_id, "morceau sans identifiant");
        return;
    };

    // Le `Sender` est clone hors du verrou : `send` attend quand le canal est
    // plein (contre-pression voulue), et attendre le verrou en main bloquerait
    // tous les autres flux du meme serveur.
    let puits = flux
        .lock()
        .await
        .get(id)
        .map(|p| (p.envoi.clone(), p.categorie));
    // La trame est entree sur le VPS, qu'elle serve ou non : elle compte.
    let categorie = puits.as_ref().map_or(Categorie::Audio, |(_, c)| *c);
    state.trafic.entree(&compteurs, categorie, taille);
    let Some((envoi, _)) = puits else {
        return;
    };

    let Some(octets) = decoder_base64(encode) else {
        // Un morceau illisible n'est pas transmis : mieux vaut un trou franc
        // qu'un octet invente au milieu du son.
        warn!(server_id = %server_id, id, "morceau base64 illisible");
        return;
    };

    if envoi.send(octets).await.is_err() {
        // Le navigateur a ferme l'onglet : le puits ne sert plus a rien.
        flux.lock().await.remove(id);
    }
}

/// Fin du flux : fermer le puits termine le corps HTTP.
async fn fermer_flux(state: &RelayState, server_id: &str, id: &str, taille: usize) {
    let (flux, compteurs) = match state.servers.get(server_id) {
        Some(conn) => (conn.flux.clone(), conn.trafic.clone()),
        None => return,
    };
    let puits = flux.lock().await.remove(id);
    let categorie = puits.as_ref().map_or(Categorie::Audio, |p| p.categorie);
    state.trafic.entree(&compteurs, categorie, taille);
    if let Some(p) = puits {
        // Avant que le `Sender` tombe : la session lira « fin propre » quand
        // son canal se fermera.
        p.fin_propre
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn resolve_pending(
    state: &RelayState,
    server_id: &str,
    resp: protocol::RelayResponse,
    taille: usize,
) {
    let (pending, compteurs) = match state.servers.get(server_id) {
        Some(conn) => (conn.pending.clone(), conn.trafic.clone()),
        None => return,
    };
    let attendu = pending.lock().await.remove(&resp.id);
    // Une reponse d'API qui porte une image est une pochette.
    let categorie = match attendu.as_ref().map(|a| a.categorie) {
        Some(Categorie::Api) | None => Categorie::d_une_reponse(
            resp.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .and_then(|(_, v)| v.as_str()),
        ),
        Some(autre) => autre,
    };
    state.trafic.entree(&compteurs, categorie, taille);
    if let Some(crate::state::Attente { tx, .. }) = attendu {
        // Un corps binaire arrive en base64 (`body_base64`), un corps texte
        // dans `body`. Un base64 illisible devient un 502 franc : mieux vaut
        // une image absente qu'une image corrompue.
        let (status, body) = match resp.body_base64 {
            Some(encode) => match decoder_base64(&encode) {
                Some(octets) => (resp.status, Some(octets)),
                None => {
                    warn!(server_id = %server_id, id = %resp.id, "corps base64 illisible");
                    (502, None)
                }
            },
            None => (resp.status, resp.body.map(String::into_bytes)),
        };
        let _ = tx.send(crate::state::PendingResponse {
            status,
            headers: resp.headers,
            body: CorpsRelaye::Entier(body),
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
    if !octets.len().is_multiple_of(4) {
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
            .register_server("srv".into(), "Salon".into(), "tok".into(), None, tx)
            .unwrap();
        rx
    }

    /// Depose l'attente qu'aurait posee `proxy_stream` juste avant d'emettre
    /// sa `relay.stream_request`.
    async fn attente_deposee(state: &RelayState, id: &str) -> oneshot::Receiver<PendingResponse> {
        let (tx, rx) = oneshot::channel::<PendingResponse>();
        let pending = state.servers.get("srv").unwrap().pending.clone();
        pending.lock().await.insert(
            id.to_string(),
            crate::state::Attente::nouvelle(tx, Categorie::Audio),
        );
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
            CorpsRelaye::Morceaux(rx, _) => rx,
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
            CorpsRelaye::Morceaux(rx, _) => rx,
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
            CorpsRelaye::Entier(Some(corps)) => assert_eq!(corps, b"{\"ok\":true}"),
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
            body: CorpsRelaye::Morceaux(
                reception,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ),
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
