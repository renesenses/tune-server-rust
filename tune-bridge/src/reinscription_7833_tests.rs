//! Un serveur qui se reconnecte avant que le relais ait vu tomber son
//! ancienne connexion (#7833) : coupure brève, box qui redémarre, nouvelle
//! adresse IP. Le serveur se dit « Connecté » ; l'app doit le trouver.
//!
//! Un vrai relais sur 127.0.0.1 avec la table de routes de `main`, de faux
//! serveurs Tune sur de vraies WebSockets, l'app jouée par un client HTTP
//! qui demande le défi d'ouverture de session.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::state::RelayState;

const SID: &str = "srv-7833";
const JETON: &str = "jeton-du-pont-7833-0123456789";
const PATIENCE: Duration = Duration::from_secs(5);

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn relais() -> (std::net::SocketAddr, Arc<RelayState>) {
    let state = Arc::new(RelayState::new());
    let app = crate::routes().with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (adresse, state)
}

/// Un faux serveur s'inscrit et attend l'accusé `relay.registered ok:true`.
async fn inscrire(pont: std::net::SocketAddr) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{pont}/ws/server"))
        .await
        .unwrap();
    let register = serde_json::json!({
        "type": "relay.register",
        "server_id": SID,
        "server_name": "Salon",
        "version": "1.0.0-rc10",
        "bridge_token": JETON,
    });
    ws.send(Message::Text(register.to_string().into()))
        .await
        .unwrap();
    let ack = tokio::time::timeout(PATIENCE, ws.next())
        .await
        .expect("pas d'accuse d'inscription")
        .unwrap()
        .unwrap();
    let ack: serde_json::Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
    assert_eq!(ack["ok"], true, "inscription refusee : {ack}");
    ws
}

/// Ce que l'app reçoit en demandant un défi : le statut et le corps.
async fn defi(pont: std::net::SocketAddr) -> (u16, String) {
    let r = reqwest::get(format!("http://{pont}/session/defi/{SID}"))
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

/// L'app doit trouver le serveur : surtout pas `404 serveur_absent`, le
/// refus qu'elle affiche « Your Tune server is not connected to Tune Bridge ».
async fn l_app_trouve_le_serveur(pont: std::net::SocketAddr, state: &RelayState, quand: &str) {
    let (statut, corps) = defi(pont).await;
    assert!(
        !(statut == 404 && corps.contains("serveur_absent")),
        "{quand} : le relais a oublie le serveur reinscrit, l'app recoit {statut} {corps}"
    );
    assert!(
        state.servers.contains_key(SID),
        "{quand} : le serveur reinscrit n'est plus dans la table du relais"
    );
}

/// Laisse au relais le temps de traiter la fin de l'ancienne connexion.
async fn laisser_le_relais_traiter() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Le cas du ticket : deux inscriptions successives, la première jamais
/// fermée par le serveur. La seconde doit rester inscrite.
#[tokio::test]
async fn une_reinscription_ne_s_efface_pas_elle_meme_7833() {
    let (pont, state) = relais().await;
    let _ancienne = inscrire(pont).await;
    let _nouvelle = inscrire(pont).await;
    laisser_le_relais_traiter().await;

    l_app_trouve_le_serveur(pont, &state, "apres la reinscription").await;
}

/// L'ancienne connexion finit par tomber (le TCP mort est enfin vu) : sa fin
/// ne doit pas emporter l'inscription de la nouvelle.
#[tokio::test]
async fn la_fin_de_l_ancienne_connexion_n_emporte_pas_la_nouvelle_7833() {
    let (pont, state) = relais().await;
    let ancienne = inscrire(pont).await;
    let _nouvelle = inscrire(pont).await;
    drop(ancienne);
    laisser_le_relais_traiter().await;

    l_app_trouve_le_serveur(pont, &state, "apres la fin de l'ancienne connexion").await;
}

/// La connexion remplacée est fermée par le relais : elle ne porte plus rien
/// et ne doit pas rester ouverte indéfiniment.
#[tokio::test]
async fn le_relais_ferme_la_connexion_remplacee_7833() {
    let (pont, _state) = relais().await;
    let mut ancienne = inscrire(pont).await;
    let _nouvelle = inscrire(pont).await;

    let fermee = tokio::time::timeout(PATIENCE, async {
        loop {
            match ancienne.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(
        fermee.is_ok(),
        "la connexion remplacee reste ouverte apres la reinscription"
    );
}

/// Témoin de la règle d'avant, toujours vraie : la fin de la SEULE connexion
/// d'un serveur le retire du relais.
#[tokio::test]
async fn la_fin_de_la_seule_connexion_retire_le_serveur_7833() {
    let (pont, state) = relais().await;
    let mut seule = inscrire(pont).await;
    seule.close(None).await.unwrap();
    laisser_le_relais_traiter().await;

    assert!(!state.servers.contains_key(SID));
    assert_eq!(state.server_for_token(JETON), None);
    let (statut, corps) = defi(pont).await;
    assert_eq!(statut, 404, "{corps}");
}
