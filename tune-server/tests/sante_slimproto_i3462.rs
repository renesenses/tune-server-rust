//! #3462 — un SlimProto mort au démarrage doit se voir depuis un écran.
//!
//! Belkadi Yacine fait tourner un Lyrion/LMS sur la même machine que Tune. Le
//! bind de SlimProto sur 3483 échoue, **aucune platine Squeezebox ne verra
//! jamais Tune**, et le serveur continue comme si de rien n'était : la panne ne
//! vivait que dans une ligne de journal d'une tâche détachée, plus dans un état
//! de session que seuls `/system/diagnostics/network` et le rapport de bogue
//! servaient — deux chemins qu'on n'emprunte qu'après avoir déjà soupçonné
//! quelque chose. Le testeur, lui, regarde la grille de composants de l'écran
//! Diagnostics et de l'onglet Système.
//!
//! Ce fichier vit à part parce que `tune_core::slimproto::etat_ecoute()` est un
//! état **global au processus** : le poser depuis un test contaminerait tous
//! les autres tests du même binaire. Une cible de test = un processus.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : sans l'entrée
//! `[[test]]` correspondante, ce fichier ne serait JAMAIS compilé et cette
//! garde serait verte contre rien.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn sante(app: &axum::Router) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/system/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Le parcours complet, dans l'ordre : rien à dire, puis un port tenu par un
/// autre serveur, puis ce que l'écran reçoit.
///
/// Les deux moitiés sont dans la MÊME épreuve parce que l'état est global :
/// séparées en deux tests, l'ordre d'exécution déciderait du résultat.
#[tokio::test]
async fn un_slimproto_hors_service_apparait_dans_les_composants_de_sante() {
    // ---- 1. Témoin d'origine : aucune tentative d'écoute, aucun composant.
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    let (code, corps) = sante(&app).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        corps["components"]["slimproto"].is_null(),
        "tant qu'aucune écoute n'a été tentée, l'absence reste une absence — \
         elle ne devient pas « en panne » : {corps}"
    );
    assert_eq!(corps["status"], "ok");

    // ---- 2. Un autre serveur tient le port : c'est la situation du testeur.
    let squatteur = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = squatteur.local_addr().unwrap().port();

    let serveur = Arc::new(tune_core::slimproto::SlimProtoServer::new_sur_port(port));
    serveur
        .spawn()
        .await
        .expect_err("le bind devait échouer : le port est déjà tenu");

    // ---- 3. Ce que l'écran reçoit.
    let (code, corps) = sante(&app).await;
    assert_eq!(
        corps["components"]["slimproto"],
        Value::Bool(false),
        "le sous-système est mort et l'écran doit pouvoir le dire : {corps}"
    );

    // ⚠️ La moitié qui compte autant que l'autre. `status` et le 503 énoncent
    // l'état de la BASE ; un LMS voisin n'a jamais empêché Tune de servir sa
    // bibliothèque. Faire basculer tout le serveur en « degraded » pour ça
    // allumerait la pastille orange de la barre latérale chez tous ceux qui
    // font tourner un Lyrion — un faux rouge permanent.
    assert_eq!(
        corps["status"], "ok",
        "un SlimProto hors service ne dégrade pas le verdict global : {corps}"
    );
    assert_eq!(code, StatusCode::OK);
    assert_eq!(corps["db"], "connected");
    assert_eq!(
        corps["components"]["db_tracks"],
        Value::Bool(true),
        "les sondes d'origine ne bougent pas : {corps}"
    );

    drop(squatteur);
}

async fn json_route(app: &axum::Router, path: &str) -> Value {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn attendre_etat(
    lire: fn() -> Option<tune_core::slimproto::EtatEcoute>,
) -> tune_core::slimproto::EtatEcoute {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(etat) = lire() {
                return etat;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("une tentative réelle doit retenir son état")
}

/// Un seul parcours ordonné pour les deux états globaux. Les sockets sont
/// réelles, les ports éphémères et aucune variable d'environnement n'est modifiée.
#[tokio::test]
async fn cli_udp_conflit_reprise_arret_sont_visibles_sans_degrader_la_base() {
    use tune_core::slimproto::{cli_server, discovery};
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);
    let (_, initial) = sante(&app).await;
    assert!(initial["components"]["lms_cli"].is_null());
    assert!(initial["components"]["slimproto_udp"].is_null());
    let cli = Arc::new(cli_server::CliState {
        players: Default::default(),
        server_name: "Tune-test".into(),
        server_version: "test".into(),
        local_ip: "127.0.0.1".into(),
    });
    let identite = || discovery::IdentiteServeur {
        nom: "Tune-test".into(),
        port_http: 8888,
        port_cli: 9090,
        version: "test".into(),
    };
    let tcp = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port_tcp = tcp.local_addr().unwrap().port();
    let udp = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let port_udp = udp.local_addr().unwrap().port();
    cli_server::start_cli_server_sur_port(cli.clone(), port_tcp).await;
    discovery::spawn_sur_port(identite(), port_udp);
    let udp_echec = attendre_etat(discovery::etat_ecoute).await;
    assert!(
        !udp_echec.ecoute,
        "un port UDP occupé doit retenir son échec"
    );
    for (nom, port, proto, variable, etat) in [
        (
            "lms_cli",
            port_tcp,
            "TCP",
            "TUNE_CLI_PORT",
            cli_server::etat_ecoute().unwrap(),
        ),
        (
            "slimproto_udp",
            port_udp,
            "UDP",
            "TUNE_SLIMPROTO_PORT",
            udp_echec,
        ),
    ] {
        assert_eq!(etat.port, port);
        assert_eq!(etat.cause, Some("adresse_deja_utilisee"));
        assert_eq!(etat.protocole, proto);
        assert!(etat.message.as_ref().unwrap().contains(variable));
        assert!(etat.erreur_systeme.as_ref().is_some_and(|e| !e.is_empty()));
        let (code, health) = sante(&app).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(health["status"], "ok");
        assert_eq!(
            health["components"][nom], false,
            "échec {nom} absent de la santé"
        );
    }
    let network = json_route(&app, "/api/v1/system/diagnostics/network").await;
    assert_eq!(network["lms_cli"]["port"], port_tcp);
    assert_eq!(network["slimproto_udp"]["port"], port_udp);
    assert_eq!(network["lms_cli"]["ecoute"], false);
    assert_eq!(network["slimproto_udp"]["ecoute"], false);

    let report = json_route(&app, "/api/v1/system/bug-report").await;
    let report_text = report.to_string();
    assert!(
        report_text.contains("TUNE_CLI_PORT"),
        "le rapport doit nommer le contournement CLI"
    );
    assert!(report_text.contains("TUNE_SLIMPROTO_PORT"));
    assert!(report_text.contains("slimproto_udp"));
    assert!(report_text.contains("adresse_deja_utilisee"));

    // L'arrêt explicite efface aussi une ancienne panne : annonce désactivée
    // n'est pas découverte en panne.
    discovery::eteindre();
    assert!(discovery::etat_ecoute().is_none());
    drop(tcp);
    drop(udp);
    let cli_task = tokio::spawn(cli_server::start_cli_server_sur_port(cli, 0));
    // L'ancienne erreur peut être lue avant le premier poll de la tâche.
    let cli_ok = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(e) = cli_server::etat_ecoute().filter(|e| e.ecoute) {
                break e;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("la reprise CLI doit remplacer l'échec par l'écoute réelle");
    assert_ne!(
        cli_ok.port, 0,
        "le diagnostic doit annoncer le port réellement obtenu"
    );
    discovery::spawn_sur_port(identite(), 0);
    let udp_ok = attendre_etat(discovery::etat_ecoute).await;
    assert!(udp_ok.ecoute);
    assert_ne!(udp_ok.port, 0);
    // Le second armement est ignoré, même s'il demanderait un autre port.
    discovery::spawn_sur_port(identite(), port_udp);
    assert_eq!(discovery::etat_ecoute().unwrap().port, udp_ok.port);
    let (_, health) = sante(&app).await;
    assert_eq!(health["components"]["lms_cli"], true);
    assert_eq!(health["components"]["slimproto_udp"], true);
    assert!(cli_ok.cause.is_none() && cli_ok.erreur_systeme.is_none());

    // Prouver le service rendu, pas seulement le booléen.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", cli_ok.port))
        .await
        .unwrap();
    socket.write_all(b"version ?\n").await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::io::BufReader::new(socket).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(line.trim(), "version test");
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    probe
        .send_to(b"eNAME\0", ("127.0.0.1", udp_ok.port))
        .await
        .unwrap();
    let mut response = [0; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        probe.recv_from(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(response[..n].windows(9).any(|w| w == b"Tune-test"));
    cli_task.abort();
    assert!(cli_task.await.unwrap_err().is_cancelled());
    discovery::eteindre();
    let (_, health) = sante(&app).await;
    assert!(health["components"]["lms_cli"].is_null());
    assert!(health["components"]["slimproto_udp"].is_null());
    assert_eq!(health["status"], "ok");
}

fn faux_lms(reponses: Vec<(&'static str, &'static str)>) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || {
        for (commande, reponse) in reponses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "commande LMS non reçue : {commande}"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Sous Windows et macOS, le socket accepté HÉRITE du mode non bloquant de
            // l'écouteur (Linux rend un socket bloquant) : `read_line` rendrait
            // `WouldBlock` si la commande n'est pas encore arrivée (#4531).
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut ligne = String::new();
            std::io::BufReader::new(socket.try_clone().unwrap())
                .read_line(&mut ligne)
                .unwrap();
            assert_eq!(ligne.trim(), commande);
            writeln!(socket, "{reponse}").unwrap();
        }
    });
    (port, worker)
}

fn app_lms(port: u16) -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set("lms_host", &format!("127.0.0.1:{port}"))
        .unwrap();
    settings.set("squeezebox_enabled", "true").unwrap();
    tune_server::routes::router(state)
}

#[tokio::test]
async fn lms_joignable_sans_platine_explique_le_refus_de_decouverte() {
    let (port, worker) = faux_lms(vec![
        ("serverstatus 0 100", "serverstatus 0 100 players:0"),
        ("player count ?", "player count 0"),
        ("player count ?", "player count 0"),
    ]);
    let app = app_lms(port);
    let status = json_route(&app, "/api/v1/squeezebox/status").await;
    assert_eq!(status["status"], "ok");
    assert_eq!(status["players"], serde_json::json!([]));
    assert_eq!(status["diagnostic"]["code"], "lms_sans_platine");
    let response = app
        .oneshot(
            Request::post("/api/v1/squeezebox/discover")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    worker.join().unwrap();
    assert_eq!(
        code,
        StatusCode::CONFLICT,
        "une découverte LMS sans platine ne doit plus rendre un succès vide : {body}"
    );
    assert_eq!(body["code"], "lms_sans_platine");
    assert_eq!(body["discovered"], 0);
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("répond")
            && message.contains("aucune platine")
            && message.contains("HQPlayer")
    );
}

#[tokio::test]
async fn une_reponse_lms_invalide_ne_devient_pas_zero_platine() {
    for reponse in ["player count ?", "garbage 0"] {
        let (port, worker) = faux_lms(vec![
            ("serverstatus 0 100", "serverstatus 0 100"),
            ("player count ?", reponse),
            ("player count ?", reponse),
        ]);
        let app = app_lms(port);
        let status = json_route(&app, "/api/v1/squeezebox/status").await;
        assert_eq!(status["diagnostic"]["code"], "lms_recensement_impossible");
        let response = app
            .oneshot(
                Request::post("/api/v1/squeezebox/discover")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn une_platine_lms_reste_decouvrable_et_enregistree() {
    let (port, worker) = faux_lms(vec![
        ("serverstatus 0 100", "serverstatus 0 100 players:1"),
        ("player count ?", "player count 1"),
        ("player id 0 ?", "player id 0 00:04:20:ab:cd:ef"),
        ("player name 0 ?", "player name 0 Salon"),
        ("player count ?", "player count 1"),
        ("player id 0 ?", "player id 0 00:04:20:ab:cd:ef"),
        ("player name 0 ?", "player name 0 Salon"),
    ]);
    let app = app_lms(port);
    let status = json_route(&app, "/api/v1/squeezebox/status").await;
    assert!(status["diagnostic"].is_null());
    assert_eq!(status["players"][0]["name"], "Salon");
    let response = app
        .oneshot(
            Request::post("/api/v1/squeezebox/discover")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    worker.join().unwrap();
    assert_eq!(body["discovered"], 1);
    assert_eq!(body["players"][0]["id"], "00:04:20:ab:cd:ef");
    assert!(body["error"].is_null());
}
