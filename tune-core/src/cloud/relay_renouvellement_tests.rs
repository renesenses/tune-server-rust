//! Renouvellement du jeton de pont, contre un faux relais local.
//!
//! Le faux relais tient sa table des jetons comme `tune-bridge`
//! (`RelayState::tokens`) : une inscription y range le jeton présenté, la
//! fermeture de la connexion du serveur retire tous ses jetons
//! (`unregister_server`). Un client du pont n'est servi que si son jeton est
//! dans la table (`server_for_token`).
//!
//! Ce qui est vérifié : après `renouveler_jeton`, le client referme la session
//! ouverte avec l'ancien jeton et se réinscrit avec le nouveau — l'ancien
//! n'est plus dans la table, le nouveau y est.
//!
//! Les jetons employés ici sont FAUX.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

const PATIENCE: Duration = Duration::from_secs(10);

/// La table des jetons du faux relais (jeton → server_id), et le compte des
/// inscriptions reçues.
#[derive(Default)]
struct Table {
    jetons: HashMap<String, String>,
    inscriptions: usize,
}

type Partagee = Arc<StdMutex<Table>>;

/// Démarre le faux relais ; rend son adresse `ws://…` et sa table.
async fn faux_relais() -> (String, Partagee) {
    let table: Partagee = Arc::default();
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("ws://{}", ecoute.local_addr().unwrap());
    let t = table.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = ecoute.accept().await else {
                return;
            };
            let t = t.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                use tokio_tungstenite::tungstenite::Message;
                let Some(Ok(Message::Text(texte))) = ws.next().await else {
                    return;
                };
                let v: serde_json::Value = serde_json::from_str(&texte).unwrap();
                assert_eq!(v["type"], "relay.register");
                let server_id = v["server_id"].as_str().unwrap().to_string();
                let jeton = v["bridge_token"].as_str().unwrap().to_string();
                {
                    let mut t = t.lock().unwrap();
                    t.jetons.insert(jeton, server_id.clone());
                    t.inscriptions += 1;
                }
                let ack = serde_json::json!({"type": "relay.registered", "ok": true});
                let _ = ws.send(Message::Text(ack.to_string().into())).await;
                // Jusqu'à la fermeture, côté serveur ou réseau.
                while let Some(Ok(m)) = ws.next().await {
                    if matches!(m, Message::Close(_)) {
                        break;
                    }
                }
                // `unregister_server` : tous les jetons de ce serveur s'en vont.
                t.lock().unwrap().jetons.retain(|_, sid| sid != &server_id);
            });
        }
    });
    (adresse, table)
}

/// Ce que le pont répondrait à un client qui présente `jeton`.
fn accepte(table: &Partagee, jeton: &str) -> bool {
    table.lock().unwrap().jetons.get(jeton).map(String::as_str) == Some("srv")
}

/// Attend que `condition` soit vraie, au plus [`PATIENCE`].
async fn attendre(quoi: &str, mut condition: impl FnMut() -> bool) {
    let debut = std::time::Instant::now();
    while !condition() {
        assert!(debut.elapsed() < PATIENCE, "délai dépassé : {quoi}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn apres_renouvellement_l_ancien_jeton_est_oublie_et_le_nouveau_accepte() {
    let (adresse, table) = faux_relais().await;
    let client = Arc::new(RelayClient::new(
        "srv".into(),
        "FAUX-ancien-jeton".into(),
        adresse,
        0,
    ));
    client.clone().spawn();

    attendre("première inscription", || {
        client.is_connected() && accepte(&table, "FAUX-ancien-jeton")
    })
    .await;

    client.renouveler_jeton("FAUX-nouveau-jeton".into());

    attendre("réinscription avec le nouveau jeton", || {
        accepte(&table, "FAUX-nouveau-jeton") && !accepte(&table, "FAUX-ancien-jeton")
    })
    .await;
    assert_eq!(
        table.lock().unwrap().inscriptions,
        2,
        "une seule réinscription attendue"
    );
    attendre("client de nouveau connecté", || client.is_connected()).await;

    // La session au nouveau jeton tient : aucun nouvel aller-retour.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t = table.lock().unwrap();
    assert_eq!(
        t.inscriptions, 2,
        "la session au nouveau jeton a été refermée"
    );
    assert_eq!(
        t.jetons.len(),
        1,
        "la table ne doit garder que le nouveau jeton"
    );
}

#[tokio::test]
async fn hors_connexion_la_prochaine_inscription_prend_le_nouveau_jeton() {
    // Le client n'est pas lancé : seul le jeton change, aucune session n'est
    // ouverte. La première inscription doit présenter le nouveau.
    let (adresse, table) = faux_relais().await;
    let client = Arc::new(RelayClient::new(
        "srv".into(),
        "FAUX-ancien-jeton".into(),
        adresse,
        0,
    ));
    client.renouveler_jeton("FAUX-nouveau-jeton".into());
    client.clone().spawn();

    attendre("inscription", || accepte(&table, "FAUX-nouveau-jeton")).await;
    assert!(!accepte(&table, "FAUX-ancien-jeton"));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        table.lock().unwrap().inscriptions,
        1,
        "un renouvellement antérieur à la session ne doit pas la refermer"
    );
}
