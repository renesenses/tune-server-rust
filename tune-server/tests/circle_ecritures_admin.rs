//! Tune Circle (décision du 19/11/2026), par le vrai câblage de l'hôte : les
//! écritures du greffon `circle` exigent un profil administrateur quand
//! l'authentification est active, et une requête relayée ne fait que lire.
//!
//! Aucun réseau : sans session SSO, le greffon répond lui-même (`GET /` :
//! `{ "connected": false }` ; une écriture : 412 `circle.not_connected`).
//! Un 412 prouve donc que la requête a passé la garde.
#![cfg(feature = "circle")]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret-cercle";

async fn demarrer(chemin: &str) -> (AppState, axum::Router) {
    use_scratch_plugin_data_dir();
    let state = AppState::new(chemin, 0, Default::default()).unwrap();
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router_with_plugins(state.clone(), routers).layer(
        axum::extract::connect_info::MockConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            50000,
        ))),
    );
    (state, app)
}

/// Un serveur où le greffon est installé (installer, puis redémarrer).
async fn serveur_avec_le_cercle(dossier: &tempfile::TempDir) -> (AppState, axum::Router) {
    let base = dossier.path().join("tune.db");
    let base = base.to_str().unwrap();
    let (state, app) = demarrer(base).await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/plugins/circle/install")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let r = app.clone().oneshot(req).await.unwrap();
    let statut = r.status();
    let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = String::from_utf8_lossy(&corps);
    assert_eq!(statut, StatusCode::OK, "{corps}");
    drop((state, app));
    demarrer(base).await
}

fn activer_l_authentification(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
}

fn jeton(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET).unwrap()
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    jeton: Option<&str>,
    relayee: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    if relayee {
        req = req.header(tune_core::cloud::ENTETE_RELAIS, "1");
    }
    let r = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
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

/// Des écritures de chaque famille : invitations, cercles, partage, globe,
/// messagerie, playlists, réglage local des pochettes.
const ECRITURES: &[(&str, &str)] = &[
    ("POST", "/api/v1/ext/circle/invitations/21/accept"),
    ("DELETE", "/api/v1/ext/circle/members/7"),
    ("PUT", "/api/v1/ext/circle/circles/1/sharing/library"),
    ("PUT", "/api/v1/ext/circle/circles/1/sharing/members"),
    ("DELETE", "/api/v1/ext/circle/member-of/1/show-me"),
    ("PUT", "/api/v1/ext/circle/me/location"),
    ("POST", "/api/v1/ext/circle/conversations/4/messages"),
    ("PATCH", "/api/v1/ext/circle/playlists/3"),
    ("PUT", "/api/v1/ext/circle/artwork-sharing"),
];

const LECTURES: &[&str] = &[
    "/api/v1/ext/circle",
    "/api/v1/ext/circle/contacts/locations",
];

#[tokio::test]
async fn authentification_active_seul_l_administrateur_ecrit() {
    let dossier = tempfile::tempdir().unwrap();
    let (state, app) = serveur_avec_le_cercle(&dossier).await;
    activer_l_authentification(&state);
    let admin = jeton("admin", 1);
    let membre = jeton("user", 2);

    for (m, c) in ECRITURES {
        let (statut, corps) = appel(&app, m, c, Some(&membre), false).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "profil non admin : {m} {c} {corps}"
        );
        assert_eq!(corps["code"], "circle.admin_required", "{m} {c}");
        assert_eq!(corps["message_key"], "api.circle.adminRequired", "{m} {c}");

        let (statut, corps) = appel(&app, m, c, Some(&admin), false).await;
        assert_ne!(
            statut,
            StatusCode::FORBIDDEN,
            "administrateur : {m} {c} {corps}"
        );
        assert_ne!(
            statut,
            StatusCode::UNAUTHORIZED,
            "administrateur : {m} {c} {corps}"
        );
    }
    // La garde passée, le greffon répond lui-même (pas de session SSO).
    let (statut, corps) = appel(
        &app,
        "DELETE",
        "/api/v1/ext/circle/members/7",
        Some(&admin),
        false,
    )
    .await;
    assert_eq!(statut, StatusCode::PRECONDITION_FAILED, "{corps}");
    assert_eq!(corps["code"], "circle.not_connected");

    // Sans jeton : la garde d'authentification répond, comme ailleurs.
    let (statut, _) = appel(&app, "DELETE", "/api/v1/ext/circle/members/7", None, false).await;
    assert_eq!(statut, StatusCode::UNAUTHORIZED);

    // Les lectures restent ouvertes à tout profil.
    for c in LECTURES {
        let (statut, corps) = appel(&app, "GET", c, Some(&membre), false).await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "GET {c} {corps}");
        assert_ne!(statut, StatusCode::UNAUTHORIZED, "GET {c} {corps}");
    }
    let (statut, corps) = appel(&app, "GET", "/api/v1/ext/circle", Some(&membre), false).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps, json!({ "connected": false }));

    // Écouter ou faire jouer n'écrit rien au compte : ouvert à tout profil.
    for c in [
        "/api/v1/ext/circle/playlists/3/play",
        "/api/v1/ext/circle/contacts/7/listen",
    ] {
        let (statut, corps) = appel(&app, "POST", c, Some(&membre), false).await;
        assert_ne!(corps["code"], "circle.admin_required", "POST {c}");
        assert_ne!(statut, StatusCode::UNAUTHORIZED, "POST {c}");
    }
}

#[tokio::test]
async fn authentification_coupee_le_local_ecrit_comme_avant() {
    let dossier = tempfile::tempdir().unwrap();
    let (_state, app) = serveur_avec_le_cercle(&dossier).await;
    for (m, c) in ECRITURES {
        let (statut, corps) = appel(&app, m, c, None, false).await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "{m} {c} {corps}");
    }
    let (statut, corps) = appel(&app, "DELETE", "/api/v1/ext/circle/members/7", None, false).await;
    assert_eq!(statut, StatusCode::PRECONDITION_FAILED, "{corps}");
    assert_eq!(corps["code"], "circle.not_connected");
}

#[tokio::test]
async fn a_distance_le_cercle_se_lit_et_ne_s_ecrit_pas() {
    let dossier = tempfile::tempdir().unwrap();
    let (state, app) = serveur_avec_le_cercle(&dossier).await;

    // Authentification coupée.
    for (m, c) in ECRITURES {
        let (statut, corps) = appel(&app, m, c, None, true).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "relayée : {m} {c} {corps}");
        assert_eq!(corps["code"], "circle.remote_read_only", "{m} {c}");
    }
    let (statut, corps) = appel(
        &app,
        "POST",
        "/api/v1/ext/circle/playlists/3/play",
        None,
        true,
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{corps}");
    assert_eq!(corps["code"], "circle.remote_read_only");
    for c in LECTURES {
        let (statut, corps) = appel(&app, "GET", c, None, true).await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "GET relayé {c} {corps}");
    }
    let (statut, corps) = appel(&app, "GET", "/api/v1/ext/circle", None, true).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps, json!({ "connected": false }));

    // Authentification active, jeton administrateur : toujours en lecture seule.
    activer_l_authentification(&state);
    let admin = jeton("admin", 1);
    for (m, c) in ECRITURES {
        let (statut, corps) = appel(&app, m, c, Some(&admin), true).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "relayée, admin : {m} {c} {corps}"
        );
        assert_eq!(corps["code"], "circle.remote_read_only", "{m} {c}");
    }
    let (statut, corps) = appel(&app, "GET", "/api/v1/ext/circle", Some(&admin), true).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}
