//! Diagnostic CarPlay en 5G (24/11) : un lecteur qui cesse de lire un flux
//! relaye ne doit bloquer ni l'API, ni le heartbeat du serveur.
//!
//! La boucle de lecture de la WebSocket du serveur attendait de pouvoir
//! deposer chaque morceau dans le canal (borne) du flux. Quand AVPlayer avait
//! rempli son tampon et cessait de lire, la boucle s'arretait : les reponses
//! d'API restaient dans la WebSocket, `relay.pong` aussi, et au bout de 90 s
//! le pont coupait toute la session — corps tronques, « erreur TLS » sur iOS.
//!
//! En boite noire : un vrai pont sur 127.0.0.1, un faux serveur Tune sur une
//! vraie WebSocket, un vrai client HTTP qui ouvre un flux et ne le lit plus.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::routing::{any, get};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::state::RelayState;

const JETON: &str = "jeton-du-pont-carplay-5g-0123456789";
const PATIENCE: Duration = Duration::from_secs(10);
/// Ce que pousse le serveur sur le flux que personne ne lit : bien plus que
/// ce que les tampons TCP et le canal de l'ancien pont absorbaient.
const POUSSE: usize = 64 * 1024 * 1024;
const MORCEAU: usize = 64 * 1024;

async fn pont(plafond: usize) -> (std::net::SocketAddr, Arc<RelayState>) {
    pont_regle(plafond, 5).await
}

async fn pont_regle(plafond: usize, flux_max: usize) -> (std::net::SocketAddr, Arc<RelayState>) {
    let mut state = RelayState::new();
    state.plafond_par_flux = plafond;
    state.max_streams_per_server = flux_max;
    let state = Arc::new(state);
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
        .route(
            "/api/relay/{server_id}/{*path}",
            any(crate::api_proxy::proxy_api),
        )
        .with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (adresse, state)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn faux_serveur(pont: std::net::SocketAddr) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{pont}/ws/server"))
        .await
        .unwrap();
    let register = serde_json::json!({
        "type": "relay.register",
        "server_id": "srv",
        "server_name": "Salon",
        "version": "1.0.0-rc14",
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
    let ack: serde_json::Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
    assert_eq!(ack["ok"], true, "inscription refusee : {ack}");
    ws
}

/// Le prochain message texte du pont d'un type donne.
async fn message_du_pont(
    source: &mut futures_util::stream::SplitStream<Ws>,
    type_attendu: &str,
) -> serde_json::Value {
    loop {
        let m = tokio::time::timeout(PATIENCE, source.next())
            .await
            .unwrap_or_else(|_| panic!("le pont n'a jamais envoye {type_attendu}"))
            .expect("WebSocket fermee")
            .unwrap();
        if let Message::Text(t) = m {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            if v["type"] == type_attendu {
                return v;
            }
        }
    }
}

fn binaire(id: &str, seq: u64, octets: &[u8]) -> Message {
    let mut t = vec![1u8, id.len() as u8];
    t.extend_from_slice(id.as_bytes());
    t.extend_from_slice(&seq.to_be_bytes());
    t.extend_from_slice(octets);
    Message::Binary(t.into())
}

/// LE TEMOIN. Un lecteur ouvre un flux et ne le lit plus ; le serveur pousse
/// la piste a son debit. Une requete d'API concurrente doit avoir sa reponse,
/// et le `relay.pong` qui suit doit etre lu — l'ancien pont restait bloque sur
/// le depot du morceau suivant, pour toute la connexion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_lecteur_qui_ne_lit_plus_ne_bloque_ni_l_api_ni_le_heartbeat() {
    let (adresse, state) = pont(4 * 1024 * 1024).await;
    let (sink, mut source) = faux_serveur(adresse).await.split();
    let battement_initial = state.servers.get("srv").unwrap().last_heartbeat;

    // 1. Le lecteur ouvre le flux, lit l'en-tete, puis plus rien.
    let (en_tete_tx, en_tete_rx) = tokio::sync::oneshot::channel::<u16>();
    let lecteur = tokio::spawn(async move {
        let r = reqwest::get(format!(
            "http://{adresse}/stream/relay/srv/abc?token={JETON}"
        ))
        .await
        .unwrap();
        let _ = en_tete_tx.send(r.status().as_u16());
        tokio::time::sleep(Duration::from_secs(120)).await;
        drop(r);
    });
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let flux = demande["id"].as_str().unwrap().to_string();

    // 2. Le serveur pousse la piste, puis son pong, puis la reponse d'API.
    let (api_tx, api_rx) = tokio::sync::oneshot::channel::<String>();
    let flux_serveur = flux.clone();
    let _serveur = tokio::spawn(async move {
        let mut sink = sink;
        let debut = serde_json::json!({
            "type": "relay.stream_start",
            "id": flux_serveur,
            "status": 200,
            "headers": {"content-type": "audio/flac"},
        });
        sink.send(Message::Text(debut.to_string().into()))
            .await
            .unwrap();
        let octets = vec![0x5Au8; MORCEAU];
        for seq in 0..(POUSSE / MORCEAU) as u64 {
            if sink
                .send(binaire(&flux_serveur, seq, &octets))
                .await
                .is_err()
            {
                return;
            }
        }
        let pong = serde_json::json!({"type": "relay.pong"});
        let _ = sink.send(Message::Text(pong.to_string().into())).await;
        let Ok(id) = api_rx.await else { return };
        let reponse = serde_json::json!({
            "type": "relay.response",
            "id": id,
            "status": 200,
            "headers": {"content-type": "application/json"},
            "body": "{\"zones\":[]}",
        });
        let _ = sink.send(Message::Text(reponse.to_string().into())).await;
    });
    assert_eq!(
        tokio::time::timeout(PATIENCE, en_tete_rx)
            .await
            .unwrap()
            .unwrap(),
        200
    );

    // 3. Une requete d'API concurrente, pendant que le flux n'est plus lu.
    let api = tokio::spawn(async move {
        let r = reqwest::get(format!(
            "http://{adresse}/api/relay/srv/api/v1/zones?token={JETON}"
        ))
        .await
        .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    });
    let requete = message_du_pont(&mut source, "relay.request").await;
    let _ = api_tx.send(requete["id"].as_str().unwrap().to_string());

    let (statut, corps) = tokio::time::timeout(PATIENCE, api)
        .await
        .expect("la requete d'API reste bloquee derriere le flux que personne ne lit")
        .unwrap();
    assert_eq!(statut, 200);
    assert_eq!(corps, "{\"zones\":[]}");

    // 4. Le pong a ete lu : le heartbeat ne coupera pas la session.
    let fin = tokio::time::Instant::now() + PATIENCE;
    while state.servers.get("srv").unwrap().last_heartbeat == battement_initial {
        assert!(
            tokio::time::Instant::now() < fin,
            "relay.pong jamais lu : le heartbeat couperait la session au bout de 90 s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 5. Le flux qui ne se lisait plus est coupe, lui seul ; le serveur
    //    reste inscrit.
    let puits = state.servers.get("srv").unwrap().flux.clone();
    assert!(
        !puits.lock().await.contains_key(&flux),
        "le flux en depassement aurait du etre coupe"
    );
    assert!(
        state.servers.contains_key("srv"),
        "la session doit rester ouverte"
    );
    assert_eq!(
        state.budget_flux.pris(),
        0,
        "la memoire du flux coupe est rendue, alors que le lecteur tient encore sa reponse"
    );
    lecteur.abort();
}

/// La memoire du pont est bornee aussi par le nombre de flux en vol :
/// `max_streams_per_server` s'applique (il etait declare, jamais lu).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn au_dela_de_max_streams_per_server_le_flux_est_refuse() {
    let (adresse, _state) = pont_regle(4 * 1024 * 1024, 1).await;
    let (mut sink, mut source) = faux_serveur(adresse).await.split();
    let (en_tete_tx, en_tete_rx) = tokio::sync::oneshot::channel::<u16>();
    let premier = tokio::spawn(async move {
        let r = reqwest::get(format!("http://{adresse}/stream/relay/srv/a?token={JETON}"))
            .await
            .unwrap();
        let _ = en_tete_tx.send(r.status().as_u16());
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(r);
    });
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let debut = serde_json::json!({
        "type": "relay.stream_start",
        "id": demande["id"],
        "status": 200,
        "headers": {"content-type": "audio/flac"},
    });
    sink.send(Message::Text(debut.to_string().into()))
        .await
        .unwrap();
    // L'en-tete du premier est rendu : son puits existe.
    assert_eq!(
        tokio::time::timeout(PATIENCE, en_tete_rx)
            .await
            .unwrap()
            .unwrap(),
        200
    );

    // Le second est refuse tout de suite, sans rien demander au serveur.
    let second = tokio::time::timeout(
        PATIENCE,
        reqwest::get(format!("http://{adresse}/stream/relay/srv/b?token={JETON}")),
    )
    .await
    .expect("le second flux attend le serveur au lieu d'etre refuse")
    .unwrap();
    assert_eq!(second.status().as_u16(), 503);
    premier.abort();
}
