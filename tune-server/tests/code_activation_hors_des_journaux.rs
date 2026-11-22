//! Le code d'activation de l'authentification ne sort par AUCUN lecteur de
//! journal de l'API.
//!
//! Le serveur l'écrit sur sa console seulement ; mais une sortie d'erreur
//! redirigée dans le fichier journal (tâche planifiée d'un NAS,
//! `>> tune.log 2>&1`), ou le repli `journalctl`, le ramène dans ce que l'API
//! sert. Cible à part : elle pose `TUNE_LOG_FILE` pour tout le processus.

use std::io::Write;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use tune_server::state::AppState;

const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";

async fn lire(state: &AppState, methode: Method, chemin: &str) -> (StatusCode, String) {
    let pair: SocketAddr = AUTRE_HOTE_DU_LAN.parse().unwrap();
    let app = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let resp = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(chemin)
                .body(Body::empty())
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

#[tokio::test]
async fn aucun_lecteur_de_journal_ne_rend_le_code() {
    let dossier = tempfile::tempdir().unwrap();
    let journal = dossier.path().join("tune-server.log");
    // Safety: seule écriture de cette variable, avant tout état.
    unsafe { std::env::set_var("TUNE_LOG_FILE", &journal) };
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();

    // Demandé depuis le réseau, comme le ferait un tiers.
    let (st, _) = lire(&state, Method::POST, "/api/v1/auth/activation/code").await;
    assert_eq!(st, StatusCode::OK);
    let code = tune_server::auth::code_activation_en_cours(&state).unwrap();

    // La console redirigée dans le fichier journal, au milieu d'autres lignes.
    let mut f = std::fs::File::create(&journal).unwrap();
    writeln!(
        f,
        "2026-11-18T10:00:00Z  INFO tune_server: demarrage temoin-avant"
    )
    .unwrap();
    writeln!(
        f,
        "{} {code} — Tune : code d'activation de l'authentification",
        tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION
    )
    .unwrap();
    writeln!(
        f,
        "2026-11-18T10:00:01Z ERROR tune_server: {} {code} ERROR",
        tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION
    )
    .unwrap();
    writeln!(
        f,
        "2026-11-18T10:00:02Z ERROR tune_server: panne temoin-apres"
    )
    .unwrap();
    drop(f);

    for (m, chemin) in [
        (Method::GET, "/api/v1/system/logs"),
        (Method::GET, "/api/v1/system/logs?lines=5000"),
        (Method::GET, "/api/v1/system/bug-report"),
        (Method::GET, "/api/v1/system/bug-report/markdown"),
        (Method::GET, "/api/v1/system/diagnostics/bundle"),
        (Method::GET, "/api/v1/system/admin/errors"),
    ] {
        let (st, corps) = lire(&state, m.clone(), chemin).await;
        assert!(
            !corps.contains(&code),
            "{m} {chemin} ({st}) rend le code : {corps}"
        );
    }
    // Témoins : le journal est bien lu, seules les lignes du code partent.
    let (_, corps) = lire(&state, Method::GET, "/api/v1/system/logs").await;
    assert!(corps.contains("temoin-avant"), "{corps}");
    assert!(corps.contains("temoin-apres"), "{corps}");
    let (_, corps) = lire(&state, Method::GET, "/api/v1/system/admin/errors").await;
    assert!(corps.contains("temoin-apres"), "{corps}");
}
