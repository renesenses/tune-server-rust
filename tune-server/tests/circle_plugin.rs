//! Tune Circle, étape T1 (#5018), de l'extérieur : le greffon `circle` par le
//! vrai câblage de l'hôte.
//!
//! Témoin 9 : installé par la route existante (`POST /api/v1/plugins/circle/
//! install`), il répond sous `/api/v1/ext/circle` au démarrage suivant ;
//! désinstallé (`DELETE /api/v1/plugins/circle`), ses routes répondent 404.
//! Les trois « démarrages » partagent le même fichier de base, comme un vrai
//! serveur relancé.
//!
//! Aucun réseau : sans session SSO, le greffon répond sans appeler le cloud.
#![cfg(feature = "circle")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

/// Un démarrage : l'état sur la base `chemin`, puis `plugins::init`.
async fn demarrer(chemin: &str) -> (AppState, axum::Router) {
    use_scratch_plugin_data_dir();
    let state = AppState::new(chemin, 0, Default::default()).unwrap();
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    (state, app)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if !corps.is_empty() {
        req = req.header("content-type", "application/json");
    }
    let r = app
        .clone()
        .oneshot(req.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn installe_il_repond_desinstalle_ses_routes_rendent_404() {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let base = base.to_str().unwrap();

    // Démarrage 1 : compilé, mais dormant tant qu'on ne l'installe pas.
    let (state, app) = demarrer(base).await;
    // Sans barre finale : l'hôte redirige `…/` (308) vers la forme sans barre.
    for (m, chemin) in [
        ("GET", "/api/v1/ext/circle"),
        ("DELETE", "/api/v1/ext/circle/members/7"),
    ] {
        let (statut, _) = appel(&app, m, chemin, "").await;
        assert_eq!(statut, StatusCode::NOT_FOUND, "non installé : {m} {chemin}");
    }
    let (statut, corps) = appel(&app, "POST", "/api/v1/plugins/circle/install", "{}").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "installed");
    drop((state, app));

    // Démarrage 2 : installé, le greffon répond.
    let (state, app) = demarrer(base).await;
    for chemin in ["/api/v1/ext/circle/", "/api/v1/ext/circle"] {
        let (statut, corps) = appel(&app, "GET", chemin, "").await;
        assert_eq!(statut, StatusCode::OK, "{chemin}");
        assert_eq!(corps, json!({ "connected": false }), "{chemin}");
    }
    let (statut, corps) = appel(&app, "DELETE", "/api/v1/ext/circle/members/7", "").await;
    assert_eq!(statut, StatusCode::PRECONDITION_FAILED);
    assert_eq!(corps["code"], "circle.not_connected");
    let (statut, corps) = appel(&app, "DELETE", "/api/v1/plugins/circle", "").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "uninstalled");
    drop((state, app));

    // Démarrage 3 : désinstallé, ses routes rendent 404.
    let (_state, app) = demarrer(base).await;
    for (m, chemin) in [
        ("GET", "/api/v1/ext/circle"),
        ("POST", "/api/v1/ext/circle/invitations/inv-1/accept"),
        ("DELETE", "/api/v1/ext/circle/members/7"),
    ] {
        let (statut, _) = appel(&app, m, chemin, "").await;
        assert_eq!(statut, StatusCode::NOT_FOUND, "{m} {chemin}");
    }
}
