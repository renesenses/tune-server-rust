//! #7374 — Tune et un Lyrion/LMS sur la même machine.
//!
//! Tune ouvrait toujours le port 3483 (TCP et UDP) et le pont de commande 9090 :
//! aucun réglage ne les fermait, et un LMS installé à côté ne pouvait pas les
//! prendre. Le réglage « Serveur Squeezebox de Tune »
//! (`slimproto_server_enabled`) les ferme et les rouvre sans redémarrage.
//!
//! Ce que ce banc prouve, par la route que l'écran appelle
//! (`PATCH /api/v1/system/config`) :
//! 1. réglage rallumé : TCP, UDP et pont de commande écoutent ;
//! 2. réglage coupé : les trois ports sont RENDUS dès la réponse, et les
//!    connexions ouvertes sont fermées ;
//! 3. port TCP déjà tenu par un autre serveur : la route répond quand même,
//!    l'état d'écoute nomme la cause, Tune reste en service ; une fois le port
//!    libéré, rallumer le réglage reprend l'écoute.
//!
//! Ports éphémères choisis au lancement, jamais 3483 ni 9090 : ce banc doit
//! pouvoir tourner sur une machine où un LMS tourne vraiment. Aucun son.
//!
//! Une seule épreuve dans un fichier à part : les états d'écoute et les
//! variables de port sont globaux au processus.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : ce fichier a son
//! entrée `[[test]]`, sans quoi il ne serait jamais compilé.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn envoyer(app: &Router, requete: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(requete).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

async fn regler_serveur(app: &Router, actif: bool) -> Value {
    let (statut, corps) = envoyer(
        app,
        Request::patch("/api/v1/system/config")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "slimproto_server_enabled": actif }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "le PATCH doit être accepté : {corps}");
    corps
}

async fn reseau(app: &Router) -> Value {
    envoyer(
        app,
        Request::get("/api/v1/system/diagnostics/network")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .1
}

/// Un port libre à la fois en TCP et en UDP : le TCP et l'annonce SlimProto
/// partagent le même numéro.
async fn port_libre_tcp_et_udp() -> u16 {
    for _ in 0..50 {
        let tcp = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        if tokio::net::UdpSocket::bind(("0.0.0.0", port)).await.is_ok() {
            return port;
        }
    }
    panic!("aucun port libre en TCP et en UDP");
}

async fn tcp_libre(port: u16) -> bool {
    tokio::net::TcpListener::bind(("0.0.0.0", port)).await.is_ok()
}

async fn udp_libre(port: u16) -> bool {
    tokio::net::UdpSocket::bind(("0.0.0.0", port)).await.is_ok()
}

async fn tcp_ecoute(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// Attend qu'une condition devienne vraie, deux secondes au plus.
async fn sous_peu<F, Fut>(condition: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..40 {
        if condition().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// La connexion est-elle fermée par le serveur (fin de flux ou erreur) ?
async fn fermee_par_le_serveur(client: &mut tokio::net::TcpStream) -> bool {
    let mut tampon = [0u8; 256];
    loop {
        match tokio::time::timeout(Duration::from_secs(2), client.read(&mut tampon)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_reglage_ouvre_et_rend_les_ports_squeezebox_7374() {
    let port_slimproto = port_libre_tcp_et_udp().await;
    let port_cli = {
        let l = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    // SAFETY : seule épreuve de ce binaire, posée avant toute tâche qui lit
    // l'environnement.
    unsafe {
        std::env::set_var(tune_core::slimproto::VARIABLE_PORT, port_slimproto.to_string());
        std::env::set_var("TUNE_CLI_PORT", port_cli.to_string());
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);

    // ---- 0. Défaut : le réglage est publié actif, rien n'a changé pour les
    // installations existantes.
    assert_eq!(
        reseau(&app).await["slimproto_server_enabled"],
        json!(true),
        "le serveur Squeezebox doit rester actif par défaut"
    );

    // ---- 1. Rallumé : les trois volets écoutent.
    let reponse = regler_serveur(&app, true).await;
    assert_eq!(reponse["slimproto_server_applied"], json!(true), "{reponse}");
    assert_eq!(reponse["slimproto_server_restart_required"], json!(false));
    assert!(
        sous_peu(|| tcp_ecoute(port_slimproto)).await,
        "le serveur TCP SlimProto doit écouter"
    );
    assert!(
        sous_peu(|| tcp_ecoute(port_cli)).await,
        "le pont de commande doit écouter"
    );
    assert!(
        sous_peu(|| async { !udp_libre(port_slimproto).await }).await,
        "l'annonce UDP doit tenir son port"
    );
    let mut client_slimproto = tokio::net::TcpStream::connect(("127.0.0.1", port_slimproto))
        .await
        .unwrap();
    let mut client_cli = tokio::net::TcpStream::connect(("127.0.0.1", port_cli))
        .await
        .unwrap();
    // Laisser les deux serveurs accepter ces connexions.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ---- 2. Coupé : les ports sont rendus DÈS la réponse.
    let reponse = regler_serveur(&app, false).await;
    assert_eq!(reponse["slimproto_server_applied"], json!(true), "{reponse}");
    assert!(
        tcp_libre(port_slimproto).await,
        "réglage coupé, le port TCP SlimProto doit être rendu dès la réponse : \
         un LMS de la même machine ne peut pas le prendre (#7374)"
    );
    assert!(
        tcp_libre(port_cli).await,
        "réglage coupé, le port du pont de commande doit être rendu dès la réponse (#7374)"
    );
    assert!(
        sous_peu(|| udp_libre(port_slimproto)).await,
        "réglage coupé, le port UDP de l'annonce doit être rendu (#7374)"
    );
    assert!(
        fermee_par_le_serveur(&mut client_slimproto).await,
        "réglage coupé, une connexion SlimProto ouverte doit être fermée (#7374)"
    );
    assert!(
        fermee_par_le_serveur(&mut client_cli).await,
        "réglage coupé, une connexion au pont de commande doit être fermée (#7374)"
    );
    let etat = reseau(&app).await;
    assert_eq!(etat["slimproto_server_enabled"], json!(false), "{etat}");
    assert!(etat["slimproto"].is_null(), "rien n'est ouvert : {etat}");
    assert!(etat["lms_cli"].is_null(), "rien n'est ouvert : {etat}");
    assert!(etat["slimproto_udp"].is_null(), "rien n'est ouvert : {etat}");

    // Couper deux fois ne change rien.
    let reponse = regler_serveur(&app, false).await;
    assert_eq!(reponse["slimproto_server_applied"], json!(false), "{reponse}");

    // ---- 3. Un autre serveur (un LMS) tient le port TCP : Tune ne tombe pas,
    // et l'état le dit.
    let lms = tokio::net::TcpListener::bind(("0.0.0.0", port_slimproto))
        .await
        .unwrap();
    let reponse = regler_serveur(&app, true).await;
    assert_eq!(reponse["ok"], json!(true), "{reponse}");
    let mut etat = Value::Null;
    for _ in 0..60 {
        etat = reseau(&app).await;
        if etat["slimproto"]["ecoute"] == json!(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(etat["slimproto_server_enabled"], json!(true), "{etat}");
    assert_eq!(
        etat["slimproto"]["ecoute"],
        json!(false),
        "un port tenu par un LMS doit se lire dans l'état d'écoute : {etat}"
    );
    assert_eq!(
        etat["slimproto"]["cause"],
        json!("port_tenu_par_un_autre_serveur"),
        "{etat}"
    );
    let (statut, _) = envoyer(
        &app,
        Request::get("/api/v1/system/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "Tune reste en service");

    // ---- 4. Le LMS s'arrête : rallumer le réglage reprend l'écoute.
    drop(lms);
    let reponse = regler_serveur(&app, true).await;
    assert_eq!(reponse["slimproto_server_applied"], json!(true), "{reponse}");
    assert!(
        sous_peu(|| tcp_ecoute(port_slimproto)).await,
        "une fois le port libéré, rallumer le réglage doit reprendre l'écoute"
    );

    // Ménage : tout rendre.
    regler_serveur(&app, false).await;
    assert!(tcp_libre(port_slimproto).await);
}
