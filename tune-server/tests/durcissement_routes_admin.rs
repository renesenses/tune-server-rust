//! Routes d'administration : un témoin par route.
//!
//! - authentification active : un compte standard reçoit 403, un appel sans
//!   jeton 401, l'administrateur passe la garde ;
//! - authentification coupée : ce qui n'a rien à faire hors administration
//!   est refusé explicitement (`auth_required`), le reste passe comme avant.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const SECRET_JWT: &str = "test-jwt-secret-durcissement";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

fn tok(role: &str, id: i64) -> String {
    tune_server::auth::sign_jwt(id, role, SECRET_JWT).unwrap()
}

async fn appel(
    state: &AppState,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
) -> (StatusCode, String) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Les routes qui exigent l'administrateur. Les corps sont valides pour la
/// désérialisation : la garde, pas le corps, doit décider.
fn routes_d_administration() -> Vec<(Method, &'static str, Option<&'static str>)> {
    vec![
        // A2
        (
            Method::GET,
            "/api/v1/system/config/export?include_secrets=true",
            None,
        ),
        // A3
        (
            Method::POST,
            "/api/v1/system/database/migrate",
            Some(r#"{"url":"postgresql://tune@192.168.1.20/tune"}"#),
        ),
        // A4
        (
            Method::POST,
            "/api/v1/appliance/storage/mount",
            Some(r#"{"uuid":"0000-0000"}"#),
        ),
        (
            Method::POST,
            "/api/v1/appliance/install-to-disk",
            Some(r#"{"device":"sdb","confirm":"EFFACER"}"#),
        ),
        (
            Method::POST,
            "/api/v1/appliance/data/relocate",
            Some(r#"{"uuid":"0000-0000"}"#),
        ),
        (
            Method::POST,
            "/api/v1/appliance/wifi/connect",
            Some(r#"{"ssid":"Maison","password":"x"}"#),
        ),
        (
            Method::POST,
            "/api/v1/appliance/wifi/forget",
            Some(r#"{"ssid":"Maison"}"#),
        ),
        // A5
        (Method::POST, "/api/v1/cloud/sso/disconnect", None),
        // B4
        (Method::GET, "/api/v1/multi-server/servers", None),
        (
            Method::POST,
            "/api/v1/multi-server/servers",
            Some(r#"{"name":"a","server_id":"b","bridge_token":"c"}"#),
        ),
        (Method::DELETE, "/api/v1/multi-server/servers/b", None),
        (Method::GET, "/api/v1/multi-server/servers/b/zones", None),
        (
            Method::GET,
            "/api/v1/multi-server/servers/b/library/albums",
            None,
        ),
        (
            Method::POST,
            "/api/v1/multi-server/servers/b/proxy",
            Some(r#"{"method":"GET","path":"/api/v1/zones"}"#),
        ),
        (Method::GET, "/api/v1/multi-server/unified/zones", None),
        // C1
        (
            Method::POST,
            "/api/v1/system/import/roon",
            Some(r#"{"data":[]}"#),
        ),
        (Method::POST, "/api/v1/system/import/plex", Some(r#"{}"#)),
        (
            Method::POST,
            "/api/v1/system/import/m3u-plays",
            Some(r#"{}"#),
        ),
        (Method::POST, "/api/v1/system/import/playlists", None),
        (
            Method::POST,
            "/api/v1/system/import/jriver",
            Some(r#"{"xml_path":"/nulle-part.xml"}"#),
        ),
        (Method::GET, "/api/v1/system/import/status/x", None),
    ]
}

#[tokio::test]
async fn un_compte_standard_est_refuse_sur_chaque_route_d_administration() {
    let state = new_state();
    enable_auth(&state);
    let standard = tok("user", 2);
    for (m, chemin, corps) in routes_d_administration() {
        let (st, rep) = appel(&state, m.clone(), chemin, Some(&standard), corps).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} : {rep}");
        assert!(rep.contains("admin role required"), "{m} {chemin} : {rep}");
        let (st, rep) = appel(&state, m.clone(), chemin, None, corps).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "{m} {chemin} sans jeton : {rep}"
        );
    }
}

/// Témoin : l'administrateur passe la garde de chaque route (la réponse
/// vient ensuite du gestionnaire : 404 hors appliance, 402 sans Premium…).
#[tokio::test]
async fn l_administrateur_passe_la_garde_de_chaque_route() {
    let state = new_state();
    enable_auth(&state);
    let admin = tok("admin", 1);
    for (m, chemin, corps) in routes_d_administration() {
        let (st, rep) = appel(&state, m.clone(), chemin, Some(&admin), corps).await;
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
        assert!(!rep.contains("admin role required"), "{m} {chemin} : {rep}");
        assert!(!rep.contains("auth_required"), "{m} {chemin} : {rep}");
    }
}

/// A5 : seul le retour du SSO reste public quand l'authentification est
/// active.
#[tokio::test]
async fn seul_le_retour_du_sso_reste_public() {
    let state = new_state();
    enable_auth(&state);
    let (st, rep) = appel(
        &state,
        Method::GET,
        "/api/v1/cloud/sso/callback?error=access_denied",
        None,
        None,
    )
    .await;
    assert_ne!(st, StatusCode::UNAUTHORIZED, "callback : {rep}");
    for (m, chemin) in [
        (Method::POST, "/api/v1/cloud/sso/disconnect"),
        (Method::GET, "/api/v1/cloud/sso/status"),
        (Method::GET, "/api/v1/cloud/sso/authorize"),
        // Méthode autre que GET sur le chemin du retour.
        (Method::POST, "/api/v1/cloud/sso/callback"),
        // Un chemin qui CONTIENT le préfixe ne le devient pas.
        (Method::GET, "/api/v1/ext/x/cloud/sso/callback"),
    ] {
        let (st, rep) = appel(&state, m.clone(), chemin, None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
    }
    // La session d'un compte standard lit toujours l'état du compte relié.
    let (st, rep) = appel(
        &state,
        Method::GET,
        "/api/v1/cloud/sso/status",
        Some(&tok("user", 2)),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rep}");
}

/// A3 : authentification coupée, la base cible doit être locale.
#[tokio::test]
async fn sans_authentification_la_migration_ne_part_pas_hors_du_reseau_local() {
    let state = new_state();
    for url in [
        "postgresql://tune:x@203.0.113.5:5432/tune",
        "postgresql://tune@pg.example.com/tune",
        "postgresql://tune@192.168.1.20/tune?host=203.0.113.5",
    ] {
        let corps = format!(r#"{{"url":"{url}"}}"#);
        let (st, rep) = appel(
            &state,
            Method::POST,
            "/api/v1/system/database/migrate",
            None,
            Some(&corps),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{url} : {rep}");
        assert!(rep.contains("auth_required"), "{url} : {rep}");
    }
    // Témoin : une base du réseau local passe la garde.
    for url in [
        "postgresql://tune@192.168.1.20/tune",
        "postgresql://tune@localhost:5432/tune",
        "postgresql://tune@nas.local/tune",
    ] {
        let corps = format!(r#"{{"url":"{url}"}}"#);
        let (_, rep) = appel(
            &state,
            Method::POST,
            "/api/v1/system/database/migrate",
            None,
            Some(&corps),
        )
        .await;
        assert!(!rep.contains("auth_required"), "{url} : {rep}");
    }
}

/// Sans authentification, les routes d'administration autres que l'export en
/// clair et la migration hors réseau local passent la garde comme avant.
#[tokio::test]
async fn sans_authentification_les_autres_routes_passent_la_garde() {
    let state = new_state();
    for (m, chemin, corps) in routes_d_administration() {
        if chemin.contains("include_secrets") {
            continue;
        }
        let (st, rep) = appel(&state, m.clone(), chemin, None, corps).await;
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
        assert!(!rep.contains("admin role required"), "{m} {chemin} : {rep}");
        assert!(!rep.contains("auth_required"), "{m} {chemin} : {rep}");
    }
}
