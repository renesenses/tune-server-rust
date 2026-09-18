//! #4361 — le pont de commande LMS ne doit plus mourir d'un port occupé, et
//! son repli ne doit pas être silencieux.
//!
//! Jacques Vincent, 17/09/2026, Tune OS Fedora aarch64 : **Cockpit** écoute sur
//! 9090 (c'est le défaut de Fedora Server), le pont CLI de Tune échoue à s'y
//! lier, et toute télécommande Squeezebox — Squeeze-LX, iPeng, Material —
//! reste sans effet. Le seul témoin était une ligne de journal :
//!
//! ```text
//! WARN lms_cli_server_bind_failed error=Address already in use (os error 98) port=9090
//! ```
//!
//! Deux défauts dans une seule phrase, gardés ici tous les deux :
//!
//! 1. **la collision** — le pont doit DÉMARRER malgré un port préféré occupé ;
//! 2. **le silence** — le repli doit être DIT, et par les chemins que les
//!    écrans lisent déjà, pas seulement par le journal.
//!
//! Ce fichier vit à part pour deux raisons, et chacune se paie cher :
//!
//! - `cli_server::etat_ecoute()` est un état **global au processus** : le poser
//!   depuis un test contaminerait les autres tests du même binaire ;
//! - ce banc écrit `TUNE_CLI_PORT`, parce que la garde doit interroger
//!   `start_cli_server()` — la fonction que `background.rs` appelle réellement
//!   — et non une variante à port explicite qui fabriquerait elle-même le
//!   conflit. `std::env::set_var` est global au processus : il n'est tolérable
//!   que dans une cible à un seul test.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : sans l'entrée
//! `[[test]]` correspondante, ce fichier ne serait JAMAIS compilé et la garde
//! serait verte contre rien.
//!
//! ⚠️ Aucun port FIXE n'est utilisé — surtout pas 9090. Le « port occupé » est
//! obtenu en se liant au port 0 et en lisant le numéro obtenu : sur une machine
//! de compilation partagée, viser un numéro écrit en dur fabriquerait soit un
//! faux rouge, soit un vert qui ne prouve rien.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

use tune_core::slimproto::cli_server;
use tune_server::state::AppState;

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

/// Le parcours complet, dans l'ordre : un port pris, le démarrage tel que le
/// serveur l'appelle, le pont qui RÉPOND, puis ce que les écrans reçoivent.
///
/// Tout tient dans une seule épreuve parce que l'état d'écoute est global :
/// séparé en plusieurs tests, l'ordre d'exécution déciderait du résultat.
#[tokio::test]
async fn un_port_occupe_deplace_le_pont_lms_et_le_dit() {
    // ---- 1. La situation du testeur : un autre service tient déjà le port.
    //         Ici ce n'est pas Cockpit, c'est une socket éphémère — la
    //         propriété gardée est « ce numéro est pris », pas « c'est 9090 ».
    let squatteur = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port_pris = squatteur.local_addr().unwrap().port();

    // SAFETY : cette cible ne contient qu'un seul test, et la variable est
    // posée avant tout démarrage de pont. Personne d'autre ne lit
    // l'environnement de ce processus en parallèle.
    unsafe { std::env::set_var("TUNE_CLI_PORT", port_pris.to_string()) };

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);

    let cli = Arc::new(cli_server::CliState {
        players: Default::default(),
        server_name: "Tune-test".into(),
        server_version: "test-4361".into(),
        local_ip: "127.0.0.1".into(),
    });

    // ---- 2. Le démarrage TEL QUE LE SERVEUR L'APPELLE (`background.rs` ne
    //         connaît que cette fonction-ci). Sans le correctif, la tâche rend
    //         la main immédiatement et l'écoute n'arrive jamais.
    let pont = tokio::spawn(cli_server::start_cli_server(cli));

    let etat = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(e) = cli_server::etat_ecoute().filter(|e| e.ecoute) {
                return e;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(
        "DÉFAUT (a) : le port préféré est tenu par un autre service et le pont \
         de commande LMS n'a jamais écouté — toute télécommande Squeezebox \
         reste sans effet, exactement comme sur Tune OS Fedora face à Cockpit",
    );

    assert_ne!(
        etat.port, port_pris,
        "le pont ne peut pas écouter sur le port que le squatteur tient"
    );
    assert_eq!(etat.protocole, "TCP");

    // ---- 3. Le pont RÉPOND vraiment : être lié ne prouve pas être servi.
    let port_reel = etat.port;
    let reponse = tokio::task::spawn_blocking(move || {
        let mut flux = std::net::TcpStream::connect(("127.0.0.1", port_reel))
            .expect("le port annoncé doit accepter une connexion");
        flux.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        flux.write_all(b"version\n").unwrap();
        let mut ligne = String::new();
        BufReader::new(flux).read_line(&mut ligne).unwrap();
        ligne
    })
    .await
    .unwrap();
    assert!(
        reponse.starts_with("version "),
        "DÉFAUT (a) : le pont est lié mais ne sert pas le protocole CLI : {reponse:?}"
    );

    // ---- 4. DÉFAUT (b) : le dégradé doit être DIT, pas subi. La cause le
    //         nomme et le message porte les DEUX numéros, sinon l'utilisateur
    //         n'a aucun moyen de savoir sur quoi pointer sa télécommande.
    assert_eq!(
        etat.cause,
        Some("port_de_repli"),
        "DÉFAUT (b) : un pont déplacé sans cause nommée est un pont déplacé en silence"
    );
    let message = etat
        .message
        .clone()
        .expect("DÉFAUT (b) : un repli sans message ne dit rien à personne");
    for attendu in [
        port_pris.to_string(),
        port_reel.to_string(),
        "TUNE_CLI_PORT".to_string(),
    ] {
        assert!(
            message.contains(&attendu),
            "DÉFAUT (b) : le message du repli doit nommer {attendu} — {message}"
        );
    }

    // ---- 5. Et il doit sortir par les chemins que les écrans lisent DÉJÀ.
    let reseau = json_route(&app, "/api/v1/system/diagnostics/network").await;
    assert_eq!(reseau["lms_cli"]["port"], port_reel);
    assert_eq!(reseau["lms_cli"]["ecoute"], true);
    assert_eq!(reseau["lms_cli"]["cause"], "port_de_repli");

    let sante = json_route(&app, "/api/v1/system/health").await;
    assert_eq!(
        sante["components"]["lms_cli"], true,
        "un pont replié SERT : il ne doit pas s'annoncer en panne"
    );
    assert_eq!(sante["status"], "ok");

    // Le rapport de bogue est la pièce que le testeur JOINT au support. La
    // section markdown est ce qu'un humain y lit — assez de ne garder que le
    // JOINT JSON : il portait déjà l'état, et il n'a servi à personne.
    let rapport = json_route(&app, "/api/v1/system/bug-report").await;
    let markdown = rapport["markdown"]
        .as_str()
        .expect("le rapport de bogue porte une section markdown")
        .to_string();
    assert!(
        markdown.contains("REPLIÉ"),
        "DÉFAUT (b) : le rapport de bogue annonce « en écoute » sans dire que le \
         numéro n'est plus celui que les télécommandes visent — {markdown}"
    );
    assert!(
        markdown.contains(&port_reel.to_string()),
        "le rapport doit porter le port réellement obtenu"
    );

    pont.abort();
    drop(squatteur);
}
