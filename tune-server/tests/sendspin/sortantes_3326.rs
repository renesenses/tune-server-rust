//! #3326 — connexions initiées par le SERVEUR : Tune découvre une enceinte
//! `_sendspin._tcp` et compose vers elle.
//!
//! L'enceinte simulée ÉCOUTE (comme Voice PE ou ESPHome) ; son adresse est
//! remise au scanner mDNS de Tune par `MdnsScanner::annoncer` — le faux
//! annonceur, puisque les runners n'ont pas de multicast. Tout le reste est
//! réel : la boucle de composition branchée par le routeur, la prise
//! sortante, la séquence serveur, le rôle `player@v1`, la zone, le PCM.
use super::*;

/// Une enceinte qui écoute sur la boucle locale ; rend son adresse.
async fn enceinte_qui_ecoute() -> (tokio::net::TcpListener, u16) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    (ecoute, port)
}

/// Attend que Tune compose, et accepte la prise WebSocket.
async fn accepter(ecoute: &tokio::net::TcpListener, secondes: u64) -> Option<Socket> {
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(secondes), ecoute.accept())
        .await
        .ok()?
        .ok()?;
    Some(
        tokio_tungstenite::accept_async(tokio_tungstenite::MaybeTlsStream::Plain(tcp))
            .await
            .expect("poignee WebSocket"),
    )
}

/// Branche un scanner mDNS sur l'état du banc et lui annonce l'enceinte.
async fn annoncer(b: &Banc, port: u16) -> Arc<tune_core::discovery::mdns::MdnsScanner> {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let scanner = Arc::new(tune_core::discovery::mdns::MdnsScanner::new(tx).unwrap());
    let appareil = tune_core::discovery::sendspin::appareil_annonce(
        "127.0.0.1",
        port,
        Some("/sendspin"),
        Some("Enceinte qui ecoute"),
    );
    scanner.annoncer(&appareil.id.clone(), Some(appareil)).await;
    *b.etat.mdns_scanner.lock().unwrap() = Some(scanner.clone());
    scanner
}

use std::sync::Arc;

#[tokio::test]
async fn i3326_sortante_tune_compose_vers_l_enceinte_annoncee_et_la_fait_jouer() {
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [90; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let (ecoute, port) = enceinte_qui_ecoute().await;
    assert!(
        accepter(&ecoute, 3).await.is_none(),
        "sans annonce, Tune ne compose vers personne"
    );
    let _scanner = annoncer(&b, port).await;
    let ws = accepter(&ecoute, 10)
        .await
        .expect("Tune compose vers l'enceinte annoncee en _sendspin._tcp");
    // Même séquence que dans l'autre sens : l'enceinte parle la première,
    // Tune répond en serveur et reste l'initiateur Noise.
    let (mut p, activation) = Pair::mener(ws, &id, &lt, support_pcm16()).await;
    assert_eq!(activation["payload"]["active_roles"], json!(["player@v1"]));
    assert_eq!(p.json().await["type"], "group/update");
    p.envoyer("client/state", etat_client(json!(["volume"])))
        .await;
    let sortie = b.sortie(&id.id()).await;
    let debut = demarrer(&mut p, &sortie, &media(&b), Some(TAUX), 500).await;
    assert_eq!(debut["payload"]["player"]["codec"], "pcm");
    let mut recu = Vec::new();
    while recu.len() < b.pcm.len() {
        if let Recu::Audio { pcm, .. } = p.recevoir().await {
            recu.extend_from_slice(&pcm);
        }
    }
    assert_eq!(
        recu, b.pcm,
        "PCM recu sur la prise sortante, octet pour octet"
    );
    sortie.lock().await.stop().await.unwrap();
}

#[tokio::test]
async fn i3326_sortante_recompose_apres_coupure_mais_pas_apres_user_request() {
    let id = Identite::generer();
    let b = Banc::nouveau(&[]).await;
    let (ecoute, port) = enceinte_qui_ecoute().await;
    let _scanner = annoncer(&b, port).await;

    // Coupure sans client/goodbye : « assume restart », Tune recompose.
    let ws = accepter(&ecoute, 10).await.expect("premiere composition");
    let (p, _) = Pair::mener(ws, &id, &PskPair::sentinelle(), support_pcm16()).await;
    drop(p);
    let ws = accepter(&ecoute, 10)
        .await
        .expect("apres une coupure, Tune recompose");

    // `client/goodbye` user_request : pas de recomposition automatique.
    let (mut p, _) = Pair::mener(ws, &id, &PskPair::sentinelle(), support_pcm16()).await;
    p.envoyer("client/goodbye", json!({"reason": "user_request"}))
        .await;
    let _ = p.ws.close(None).await;
    assert!(
        accepter(&ecoute, 8).await.is_none(),
        "apres user_request, Tune ne recompose pas"
    );
}

// --- Liste d'exclusion (décision de Bertrand du 10/10/2026) -------------------

async fn exclure(b: &Banc, id: &str, exclu: bool) -> Value {
    let r = reqwest::Client::new()
        .put(format!(
            "http://{}/api/v1/devices/sendspin/exclusions/{id}",
            b.adresse
        ))
        .json(&json!({ "excluded": exclu }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "PUT exclusion : {}", r.status());
    r.json().await.unwrap()
}

async fn liste(b: &Banc) -> Value {
    reqwest::get(format!("http://{}/api/v1/devices/sendspin", b.adresse))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// `client/init` envoyé : rend la réponse du serveur, ou `None` s'il ferme.
async fn reponse_au_client_init(mut ws: Socket, id: &Identite) -> Option<String> {
    let init = json!({"type":"client/init","payload":{"client_id":id.id(),"version":1,"suite":Suite::ChaChaPoly.nom()}});
    ws.send(Message::Text(init.to_string().into())).await.ok()?;
    match tokio::time::timeout(ATTENTE, ws.next()).await.ok()? {
        Some(Ok(Message::Text(t))) => Some(t.to_string()),
        _ => None,
    }
}

#[tokio::test]
async fn i3326_exclusion_une_enceinte_exclue_qui_compose_est_refusee() {
    let id = Identite::generer();
    let lt = PskPair::pour_pair(&id.id(), [91; 32], CategoriePsk::LongueDuree).unwrap();
    let b = Banc::nouveau(&[(&id, &lt)]).await;
    let v = exclure(&b, &id.id(), true).await;
    assert_eq!(v["list"], json!([id.id()]));
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/sendspin", b.adresse))
        .await
        .unwrap();
    assert_eq!(
        reponse_au_client_init(ws, &id).await,
        None,
        "une enceinte exclue ne recoit pas meme server/init"
    );
    assert!(b.sortie_absente(&id.id()).await);
    // Réadmise : la même enceinte devient une zone.
    exclure(&b, &id.id(), false).await;
    let (mut p, activation) = Pair::ouvrir(&b, &id, &lt, support_pcm16()).await;
    assert_eq!(activation["payload"]["active_roles"], json!(["player@v1"]));
    assert_eq!(p.json().await["type"], "group/update");
}

#[tokio::test]
async fn i3326_exclusion_tune_ne_compose_pas_vers_une_annonce_exclue() {
    let b = Banc::nouveau(&[]).await;
    let (ecoute, port) = enceinte_qui_ecoute().await;
    let annonce = tune_core::discovery::sendspin::appareil_annonce(
        "127.0.0.1",
        port,
        Some("/sendspin"),
        None,
    );
    exclure(&b, &annonce.id, true).await;
    let _scanner = annoncer(&b, port).await;
    assert!(
        accepter(&ecoute, 6).await.is_none(),
        "annonce exclue : Tune ne compose pas"
    );
    let l = liste(&b).await;
    assert_eq!(l["players"][0]["excluded"], true, "{l}");
    exclure(&b, &annonce.id, false).await;
    assert!(
        accepter(&ecoute, 8).await.is_some(),
        "readmise : Tune compose de nouveau"
    );
}

#[tokio::test]
async fn i3326_exclusion_par_client_id_ferme_la_session_et_ne_recompose_plus() {
    let id = Identite::generer();
    let b = Banc::nouveau(&[]).await;
    let (ecoute, port) = enceinte_qui_ecoute().await;
    let _scanner = annoncer(&b, port).await;
    let ws = accepter(&ecoute, 10).await.expect("Tune compose");
    let (mut p, _) = Pair::mener(ws, &id, &PskPair::sentinelle(), support_pcm16()).await;
    // Le client_id appris à la composition apparaît sur l'annonce.
    let l = liste(&b).await;
    assert_eq!(l["players"][0]["client_id"], id.id(), "{l}");
    assert_eq!(l["players"][0]["excluded"], false);
    exclure(&b, &id.id(), true).await;
    let fermee = tokio::time::timeout(ATTENTE, async {
        loop {
            match p.ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(fermee.is_ok(), "la session de l'enceinte exclue est close");
    assert!(
        accepter(&ecoute, 8).await.is_none(),
        "exclue par client_id : Tune ne recompose plus"
    );
    let l = liste(&b).await;
    assert_eq!(l["players"][0]["excluded"], true, "{l}");
    assert_eq!(l["excluded"], json!([id.id()]));
}
