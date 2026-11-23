//! Le code d'activation de l'authentification est AUSSI écrit dans le fichier
//! journal local (`tune-server.log`), sur une ligne qui porte
//! `TUNE-ACTIVATION-CODE` — pour les installations sans console visible
//! (Daphile, `nohup`, certains services).
//!
//! Et il ne sort de la machine par aucun chemin : ni par les routes qui
//! servent le journal, ni par le rapport de bogue envoyé au site, ni par la
//! porte commune de ce qui part vers le site.
//!
//! Cible à part : elle installe le VRAI abonné `tracing` du serveur
//! (`bootstrap::installer_le_journal`) et pose `TUNE_LOG_FILE` pour tout le
//! processus.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";
const CETTE_MACHINE: &str = "127.0.0.1:50000";

async fn appeler(
    state: &AppState,
    pair: &str,
    methode: Method,
    chemin: &str,
    corps: Option<&str>,
) -> (StatusCode, String) {
    let pair: SocketAddr = pair.parse().unwrap();
    let app = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    if corps.is_some() {
        req = req.header("content-type", "application/json");
    }
    let resp = app
        .oneshot(
            req.body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (st, String::from_utf8_lossy(&octets).to_string())
}

/// Un faux service communautaire local : il garde ce que le rapport de bogue
/// lui envoie réellement.
async fn faux_site() -> (String, Arc<Mutex<Vec<String>>>) {
    let recus = Arc::new(Mutex::new(Vec::new()));
    let r = recus.clone();
    let app = axum::Router::new().route(
        "/api/v1/community/bug-report",
        axum::routing::post(move |corps: axum::body::Bytes| {
            let r = r.clone();
            async move {
                r.lock()
                    .await
                    .push(String::from_utf8_lossy(&corps).to_string());
                axum::Json(serde_json::json!({
                    "status": "submitted",
                    "thread": { "id": 1, "slug": "essai", "url": "https://exemple/forum/threads/essai" },
                }))
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://127.0.0.1:{port}"), recus)
}

#[tokio::test]
async fn le_code_est_dans_le_fichier_journal_et_nulle_part_ailleurs() {
    let dossier = tempfile::tempdir().unwrap();
    let journal = dossier.path().join("tune-server.log");
    // Safety: seules écritures de ces variables, avant tout état et tout fil.
    unsafe {
        std::env::set_var("TUNE_LOG_FILE", &journal);
        std::env::remove_var("RUST_LOG");
        std::env::remove_var("TUNE_LOG_LEVEL");
    }
    // Le niveau le plus discret : le code doit passer quand même, sinon
    // l'exploitant qui a réglé `error` ne le trouverait pas.
    let chemin = tune_server::bootstrap::installer_le_journal("error");
    assert_eq!(chemin.as_deref(), Some(journal.as_path()));

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let (base, recus) = faux_site().await;
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", &base)
        .unwrap();

    // Une ligne ordinaire avant, pour prouver que les lecteurs lisent bien ce
    // fichier-là.
    tracing::error!(target: "tune_server::temoin", "temoin-avant");

    // Demandé depuis le réseau.
    let (st, _) = appeler(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/activation/code",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = tune_server::auth::code_activation_en_cours(&state).unwrap();
    tracing::error!(target: "tune_server::temoin", "temoin-apres");

    // 1. Le code est dans le fichier journal local, sur la ligne marquée.
    let texte = std::fs::read_to_string(&journal).unwrap();
    let ligne = texte
        .lines()
        .find(|l| l.contains(&code))
        .unwrap_or_else(|| panic!("le code n'est pas dans le fichier journal :\n{texte}"));
    assert!(
        ligne.contains(&format!("{MARQUEUR_CODE_ACTIVATION} {code} — Tune")),
        "même format que la console attendu : {ligne}"
    );
    assert!(ligne.contains("min."), "{ligne}");
    assert_eq!(
        texte.matches(&code).count(),
        1,
        "une seule ligne porte le code :\n{texte}"
    );

    // 2. Aucune route qui sert le journal ne le rend.
    for (m, chemin) in [
        (Method::GET, "/api/v1/system/logs"),
        (Method::GET, "/api/v1/system/logs?lines=5000"),
        (Method::GET, "/api/v1/system/bug-report"),
        (Method::GET, "/api/v1/system/bug-report/markdown"),
        (Method::GET, "/api/v1/system/diagnostics/bundle"),
        (Method::GET, "/api/v1/system/admin/errors"),
    ] {
        for pair in [AUTRE_HOTE_DU_LAN, CETTE_MACHINE] {
            let (st, corps) = appeler(&state, pair, m.clone(), chemin, None).await;
            assert!(
                !corps.contains(&code) && !corps.contains(MARQUEUR_CODE_ACTIVATION),
                "{m} {chemin} depuis {pair} ({st}) rend le code"
            );
        }
    }
    // Témoins : ce fichier est bien celui que l'API lit.
    let (_, corps) = appeler(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/system/logs",
        None,
    )
    .await;
    assert!(corps.contains("temoin-avant"), "{corps}");
    assert!(corps.contains("temoin-apres"), "{corps}");

    // 3. Le rapport de bogue envoyé au site : journal joint, code retiré.
    let (st, rep) = appeler(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/system/bug-report/submit",
        Some(r#"{"description":"Essai du rapport"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    let recus = recus.lock().await;
    assert_eq!(recus.len(), 1, "le rapport n'est pas parti");
    assert!(
        recus[0].contains("temoin-apres"),
        "le journal doit être joint au rapport : {}",
        recus[0]
    );
    assert!(
        !recus[0].contains(&code) && !recus[0].contains(MARQUEUR_CODE_ACTIVATION),
        "le rapport envoyé au site porte le code"
    );

    // 4. La porte commune de tout ce qui part vers le site (ticket, rapport,
    //    assistant) retire la ligne, même si le fichier entier y passe.
    for sortie in [
        tune_core::confidentialite::masquer_acces_distant(&texte),
        tune_core::confidentialite::anonymiser(&texte),
    ] {
        assert!(!sortie.contains(&code));
        assert!(sortie.contains("temoin-apres"), "{sortie}");
    }
}
