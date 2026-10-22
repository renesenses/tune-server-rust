//! #6564 de bout en bout : un vrai pont sur 127.0.0.1, un faux serveur Tune
//! sur une vraie WebSocket, un vrai client HTTP qui lit le flux.
//!
//! En boite noire : ce qui compte est ce que le navigateur recoit, quel que
//! soit le format choisi par le serveur.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::state::RelayState;

const JETON: &str = "jeton-du-pont-6564-0123456789";
const PATIENCE: Duration = Duration::from_secs(10);

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Le pont, avec ses deux routes concernees.
async fn pont() -> std::net::SocketAddr {
    let state = Arc::new(RelayState::new());
    let app = Router::new()
        .route(
            "/ws/server",
            get(
                |ws: WebSocketUpgrade, State(s): State<Arc<RelayState>>| async move {
                    ws.on_upgrade(move |socket| crate::ws_server::handle_server_ws(socket, s))
                },
            ),
        )
        .route(
            "/stream/relay/{server_id}/{*stream_path}",
            get(crate::stream_proxy::proxy_stream),
        )
        .with_state(state);
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    adresse
}

/// Le faux serveur s'enregistre ; rend sa WebSocket et l'accuse du pont.
async fn faux_serveur(pont: std::net::SocketAddr) -> (Ws, serde_json::Value) {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{pont}/ws/server"))
        .await
        .unwrap();
    let register = serde_json::json!({
        "type": "relay.register",
        "server_id": "srv",
        "server_name": "Salon",
        "version": "1.0.0-rc6",
        "bridge_token": JETON,
    });
    ws.send(Message::Text(register.to_string().into()))
        .await
        .unwrap();
    let ack = tokio::time::timeout(PATIENCE, ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ack = serde_json::from_str(ack.to_text().unwrap()).unwrap();
    (ws, ack)
}

/// Le format binaire, ecrit a la main d'apres la doc du module.
fn binaire(id: &str, seq: u64, octets: &[u8]) -> Message {
    let mut t = vec![1u8, id.len() as u8];
    t.extend_from_slice(id.as_bytes());
    t.extend_from_slice(&seq.to_be_bytes());
    t.extend_from_slice(octets);
    Message::Binary(t.into())
}

/// L'ancien format, celui des serveurs rc4 a rc6.
fn base64(id: &str, octets: &[u8]) -> Message {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(octets);
    Message::Text(format!("BINARY:{id}:{b64}").into())
}

#[test]
fn le_format_ecrit_ici_est_celui_de_reference() {
    assert_eq!(
        binaire("req-1", 2, b"fLaC"),
        Message::Binary(super::tests::VECTEUR.to_vec().into())
    );
}

/// 4 Mio en morceaux de tailles irregulieres ; chaque rang a son octet.
fn flux_de_test() -> Vec<Vec<u8>> {
    let mut morceaux = Vec::new();
    let mut debut = 0usize;
    while debut < 4 * 1024 * 1024 {
        let taille = 512 + (debut * 7919) % (31 * 1024);
        morceaux.push(
            (debut..debut + taille)
                .map(|i| (i.wrapping_mul(31) ^ (i >> 8) ^ (i >> 16)) as u8)
                .collect(),
        );
        debut += taille;
    }
    morceaux
}

/// Un navigateur demande `/stream/relay/srv/abc` ; le faux serveur repond par
/// `relay.stream_start`, les trames de `trames(id)`, puis `relay.stream_end`.
/// Rend le corps HTTP recu par le navigateur.
async fn relayer(
    pont: std::net::SocketAddr,
    ws: &mut Ws,
    trames: impl FnOnce(&str) -> Vec<Message>,
) -> Vec<u8> {
    let navigateur = tokio::spawn(async move {
        let r = reqwest::get(format!("http://{pont}/stream/relay/srv/abc?token={JETON}"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap().to_vec()
    });

    let demande = tokio::time::timeout(PATIENCE, ws.next())
        .await
        .expect("le pont n'a rien demande")
        .unwrap()
        .unwrap();
    let demande: serde_json::Value = serde_json::from_str(demande.to_text().unwrap()).unwrap();
    assert_eq!(demande["type"], "relay.stream_request");
    let id = demande["id"].as_str().unwrap().to_string();

    let debut = serde_json::json!({
        "type": "relay.stream_start",
        "id": id,
        "status": 200,
        "headers": {"content-type": "audio/flac"},
    });
    ws.send(Message::Text(debut.to_string().into()))
        .await
        .unwrap();
    for t in trames(&id) {
        ws.send(t).await.unwrap();
    }
    let fin = serde_json::json!({"type": "relay.stream_end", "id": id});
    ws.send(Message::Text(fin.to_string().into()))
        .await
        .unwrap();

    tokio::time::timeout(PATIENCE, navigateur)
        .await
        .expect("le corps HTTP ne s'est jamais termine : la fin de flux n'a pas ferme le puits")
        .unwrap()
}

/// Le pont annonce la capacite dans `relay.registered`.
#[tokio::test]
async fn le_pont_annonce_les_morceaux_binaires() {
    let adresse = pont().await;
    let (_ws, ack) = faux_serveur(adresse).await;
    assert_eq!(ack["ok"], true);
    assert!(
        ack["capabilities"].as_array().is_some_and(|c| c
            .iter()
            .any(|v| v == crate::protocol::CAPACITE_MORCEAUX_BINAIRES)),
        "capacite absente de l'accuse : {ack}"
    );
}

/// L'ALLER-RETOUR, sur un grand flux : tout arrive, dans l'ordre, et la fin
/// de flux termine le corps HTTP.
#[tokio::test]
async fn un_grand_flux_binaire_arrive_entier_et_dans_l_ordre() {
    let adresse = pont().await;
    let (mut ws, _) = faux_serveur(adresse).await;
    let morceaux = flux_de_test();
    let attendu: Vec<u8> = morceaux.concat();
    let recu = relayer(adresse, &mut ws, |id| {
        morceaux
            .iter()
            .enumerate()
            .map(|(seq, m)| binaire(id, seq as u64, m))
            .collect()
    })
    .await;
    assert_eq!(recu.len(), attendu.len());
    assert!(recu == attendu, "flux altere");
}

/// REPLI : un serveur rc4 a rc6 envoie toujours du base64 en trames texte,
/// et le pont le sert comme avant.
#[tokio::test]
async fn un_serveur_ancien_en_base64_est_toujours_servi() {
    let adresse = pont().await;
    let (mut ws, _) = faux_serveur(adresse).await;
    let morceaux = flux_de_test();
    let attendu: Vec<u8> = morceaux.concat();
    let recu = relayer(adresse, &mut ws, |id| {
        morceaux.iter().map(|m| base64(id, m)).collect()
    })
    .await;
    assert!(recu == attendu, "flux altere");
}

/// Un morceau perdu coupe le flux net : le navigateur recoit ce qui precede
/// le trou, et rien de recolle apres.
#[tokio::test]
async fn un_morceau_perdu_coupe_le_flux_au_lieu_de_le_recoller() {
    let adresse = pont().await;
    let (mut ws, _) = faux_serveur(adresse).await;
    let recu = relayer(adresse, &mut ws, |id| {
        vec![
            binaire(id, 0, b"fLaC"),
            binaire(id, 1, b"AB"),
            // le rang 2 manque
            binaire(id, 3, b"CD"),
            binaire(id, 4, b"EF"),
        ]
    })
    .await;
    assert_eq!(recu, b"fLaCAB");
}

/// Deux flux du meme serveur, morceaux entrelaces : chacun recoit les siens.
#[tokio::test]
async fn deux_flux_entrelaces_ne_se_melangent_pas() {
    let adresse = pont().await;
    let (mut ws, _) = faux_serveur(adresse).await;

    let lire = |n: u8| {
        tokio::spawn(async move {
            let r = reqwest::get(format!(
                "http://{adresse}/stream/relay/srv/piste-{n}?token={JETON}"
            ))
            .await
            .unwrap();
            r.bytes().await.unwrap().to_vec()
        })
    };
    let a = lire(1);
    let b = lire(2);

    // Les deux demandes, dans un ordre quelconque.
    let mut ids = std::collections::HashMap::new();
    for _ in 0..2 {
        let d = tokio::time::timeout(PATIENCE, ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let d: serde_json::Value = serde_json::from_str(d.to_text().unwrap()).unwrap();
        ids.insert(
            d["stream_id"].as_str().unwrap().to_string(),
            d["id"].as_str().unwrap().to_string(),
        );
    }
    let (ia, ib) = (ids["piste-1"].clone(), ids["piste-2"].clone());
    for id in [&ia, &ib] {
        let debut = serde_json::json!({
            "type": "relay.stream_start", "id": id, "status": 200, "headers": {},
        });
        ws.send(Message::Text(debut.to_string().into()))
            .await
            .unwrap();
    }
    for seq in 0..50u64 {
        ws.send(binaire(&ia, seq, &[b'a'; 100])).await.unwrap();
        ws.send(binaire(&ib, seq, &[b'b'; 70])).await.unwrap();
    }
    for id in [&ia, &ib] {
        let fin = serde_json::json!({"type": "relay.stream_end", "id": id});
        ws.send(Message::Text(fin.to_string().into()))
            .await
            .unwrap();
    }
    assert_eq!(
        tokio::time::timeout(PATIENCE, a).await.unwrap().unwrap(),
        vec![b'a'; 5000]
    );
    assert_eq!(
        tokio::time::timeout(PATIENCE, b).await.unwrap().unwrap(),
        vec![b'b'; 3500]
    );
}
