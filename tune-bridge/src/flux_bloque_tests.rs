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
    pont_avec(plafond, flux_max, Duration::from_secs(60)).await
}

async fn pont_avec(
    plafond: usize,
    flux_max: usize,
    inactivite: Duration,
) -> (std::net::SocketAddr, Arc<RelayState>) {
    let mut state = RelayState::new();
    state.plafond_par_flux = plafond;
    state.max_streams_per_server = flux_max;
    // vert-seul-debut
    state.delai_inactivite = inactivite;
    // vert-seul-fin
    let _ = inactivite;
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
    // Le lecteur a ses en-tetes avant que la piste deborde, comme AVPlayer.
    let (lu_tx, lu_rx) = tokio::sync::oneshot::channel::<()>();
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
        if lu_rx.await.is_err() {
            return;
        }
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
    let _ = lu_tx.send(());

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

fn texte(v: serde_json::Value) -> Message {
    Message::Text(v.to_string().into())
}

fn debut(id: &str) -> Message {
    texte(serde_json::json!({
        "type": "relay.stream_start",
        "id": id,
        "status": 200,
        "headers": {"content-type": "audio/flac"},
    }))
}

fn fin(id: &str) -> Message {
    texte(serde_json::json!({"type": "relay.stream_end", "id": id}))
}

/// Un lecteur qui ouvre un flux, rend son statut, puis ne lit plus rien.
fn lecteur_en_pause(
    adresse: std::net::SocketAddr,
    chemin: &str,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<u16>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel::<u16>();
    let url = format!("http://{adresse}/stream/relay/srv/{chemin}?token={JETON}");
    let tache = tokio::spawn(async move {
        let r = reqwest::get(url).await.unwrap();
        let _ = tx.send(r.status().as_u16());
        tokio::time::sleep(Duration::from_secs(120)).await;
        drop(r);
    });
    (tache, rx)
}

async fn obtenir(adresse: std::net::SocketAddr, chemin: &str) -> reqwest::Response {
    tokio::time::timeout(
        PATIENCE,
        reqwest::get(format!(
            "http://{adresse}/stream/relay/srv/{chemin}?token={JETON}"
        )),
    )
    .await
    .expect("la requete attend le serveur au lieu d'etre refusee")
    .unwrap()
}

/// 24 Mio sur le flux `id` : plus que les tampons TCP n'absorbent, moins que
/// le plafond du flux — la file en garde une part tant que personne ne lit.
async fn pousser(sink: &mut futures_util::stream::SplitSink<Ws, Message>, id: &str) {
    let octets = vec![0x5Au8; MORCEAU];
    for seq in 0..(24 * 1024 * 1024 / MORCEAU) as u64 {
        sink.send(binaire(id, seq, &octets)).await.unwrap();
    }
}

/// Au-dela de `max_streams_per_server`, le flux est refuse par un 503 avec
/// `Retry-After` — y compris quand la place est prise par une requete qui
/// attend encore son `stream_start` — puis un nouvel essai passe une fois la
/// place rendue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_flux_refuse_en_503_passe_au_nouvel_essai_une_fois_la_place_rendue() {
    let (adresse, _state) = pont_regle(32 * 1024 * 1024, 1).await;
    let (mut sink, mut source) = faux_serveur(adresse).await.split();
    let premier = tokio::spawn(async move {
        let r = reqwest::get(format!("http://{adresse}/stream/relay/srv/a?token={JETON}"))
            .await
            .unwrap();
        r.bytes().await.unwrap().len()
    });
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let id = demande["id"].as_str().unwrap().to_string();

    // Le premier attend encore son `stream_start` : sa place est prise.
    let refus = obtenir(adresse, "b").await;
    assert_eq!(refus.status().as_u16(), 503);
    assert_eq!(
        refus
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("2")
    );

    // Le premier flux se joue jusqu'au bout et son lecteur le lit.
    sink.send(debut(&id)).await.unwrap();
    sink.send(binaire(&id, 0, b"fLaC")).await.unwrap();
    sink.send(fin(&id)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(PATIENCE, premier)
            .await
            .unwrap()
            .unwrap(),
        4
    );

    // La place est rendue : le nouvel essai part au serveur et passe.
    let second = tokio::spawn(async move { obtenir(adresse, "b").await.status().as_u16() });
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let id = demande["id"].as_str().unwrap().to_string();
    sink.send(debut(&id)).await.unwrap();
    sink.send(fin(&id)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(PATIENCE, second)
            .await
            .unwrap()
            .unwrap(),
        200
    );
}

/// Un lecteur en pause APRES `relay.stream_end` tient encore des octets du
/// pont : il compte toujours dans la limite de son serveur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_lecteur_en_pause_apres_la_fin_du_flux_compte_encore() {
    let (adresse, _state) = pont_regle(32 * 1024 * 1024, 1).await;
    let (mut sink, mut source) = faux_serveur(adresse).await.split();
    let (lecteur, en_tete) = lecteur_en_pause(adresse, "a");
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let id = demande["id"].as_str().unwrap().to_string();
    sink.send(debut(&id)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(PATIENCE, en_tete)
            .await
            .unwrap()
            .unwrap(),
        200
    );
    pousser(&mut sink, &id).await;
    sink.send(fin(&id)).await.unwrap();
    // Le pont a lu la fin (le puits est retire)…
    tokio::time::sleep(Duration::from_millis(300)).await;

    // …mais le lecteur n'a pas tout lu : sa place reste prise.
    assert_eq!(obtenir(adresse, "b").await.status().as_u16(), 503);
    lecteur.abort();
}

/// Un corps que personne ne lit est coupe au bout du delai d'inactivite :
/// ses octets reviennent au budget et sa place a son serveur.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_delai_dinactivite_rend_le_budget_et_la_place() {
    let (adresse, state) = pont_avec(32 * 1024 * 1024, 1, Duration::from_secs(1)).await;
    let (mut sink, mut source) = faux_serveur(adresse).await.split();
    let (lecteur, en_tete) = lecteur_en_pause(adresse, "a");
    let demande = message_du_pont(&mut source, "relay.stream_request").await;
    let id = demande["id"].as_str().unwrap().to_string();
    sink.send(debut(&id)).await.unwrap();
    assert_eq!(
        tokio::time::timeout(PATIENCE, en_tete)
            .await
            .unwrap()
            .unwrap(),
        200
    );
    pousser(&mut sink, &id).await;
    sink.send(fin(&id)).await.unwrap();

    let echeance = tokio::time::Instant::now() + PATIENCE;
    while state.budget_flux.pris() > 0 {
        assert!(
            tokio::time::Instant::now() < echeance,
            "le corps non lu garde ses octets : le delai d'inactivite ne les rend pas"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // vert-seul-debut
    assert_eq!(state.quota("srv").places(), 0, "la place est rendue");
    // vert-seul-fin
    lecteur.abort();
}

/// Des requetes simultanees ne depassent pas la limite : controle et prise
/// de place sont atomiques, et une requete qui attend son `stream_start`
/// compte deja.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn des_requetes_simultanees_ne_depassent_pas_la_limite() {
    let (adresse, _state) = pont_regle(32 * 1024 * 1024, 2).await;
    let (_sink, mut source) = faux_serveur(adresse).await.split();
    let (statuts_tx, mut statuts) = tokio::sync::mpsc::unbounded_channel::<u16>();
    let mut taches = Vec::new();
    for i in 0..6 {
        let tx = statuts_tx.clone();
        taches.push(tokio::spawn(async move {
            let r = reqwest::get(format!(
                "http://{adresse}/stream/relay/srv/p{i}?token={JETON}"
            ))
            .await
            .unwrap();
            let _ = tx.send(r.status().as_u16());
        }));
    }

    // Ce que le serveur recoit pendant une seconde : deux demandes au plus.
    let mut demandes = 0;
    let fenetre = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(fenetre, source.next()).await {
        if let Message::Text(t) = m
            && serde_json::from_str::<serde_json::Value>(&t).unwrap()["type"]
                == "relay.stream_request"
        {
            demandes += 1;
        }
    }
    assert!(
        demandes <= 2,
        "{demandes} flux demandes au serveur pour une limite de 2"
    );

    // Les quatre autres sont refusees tout de suite.
    for _ in 0..4 {
        let statut = tokio::time::timeout(PATIENCE, statuts.recv())
            .await
            .expect("une requete au-dela de la limite attend au lieu d'etre refusee")
            .unwrap();
        assert_eq!(statut, 503);
    }
    taches.iter().for_each(|t| t.abort());
}
