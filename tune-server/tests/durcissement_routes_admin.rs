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
        // A3 — cible sur cette machine (port 1 : rien n'y écoute), seule
        // admise sans authentification depuis la garde « cette machine ».
        (
            Method::POST,
            "/api/v1/system/database/migrate",
            Some(r#"{"url":"postgresql://tune@localhost:1/tune"}"#),
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

/// Sécurité : authentification coupée, la base cible doit être
/// sur CETTE machine — le réseau local, qui est l'adresse même d'un client
/// du réseau, n'y suffit plus.
#[tokio::test]
async fn sans_authentification_la_migration_ne_part_pas_hors_de_cette_machine() {
    let state = new_state();
    for url in [
        "postgresql://tune:x@203.0.113.5:5432/tune",
        "postgresql://tune@pg.example.com/tune",
        "postgresql://tune@192.168.1.20/tune",
        "postgresql://tune@10.0.0.4/tune",
        "postgresql://tune@nas.local/tune",
        "postgresql://tune@nas/tune",
        "postgresql://tune@[fd00::4]/tune",
        "postgresql://tune@localhost/tune?host=192.168.1.20",
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
    // Témoin : une base de cette machine passe la garde (port 1 : rien n'y
    // écoute, la migration échoue ensuite, ailleurs que dans la garde).
    for url in [
        "postgresql://tune@localhost:1/tune",
        "postgresql://tune@127.0.0.1:1/tune",
        "postgresql:///tune?host=/nulle-part-base",
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

/// Sécurité : authentification ACTIVE, une base du réseau local
/// exige un administrateur — un compte standard est refusé, un appel sans
/// jeton aussi.
#[tokio::test]
async fn avec_authentification_la_migration_vers_le_reseau_local_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let corps = r#"{"url":"postgresql://tune@192.168.1.20/tune"}"#;
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/system/database/migrate",
        None,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{rep}");
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/system/database/migrate",
        Some(&tok("user", 2)),
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
    assert!(!rep.contains("auth_required"), "{rep}");
}

/// Sans authentification, les routes d'administration autres que l'export en
/// clair et la migration hors de cette machine passent la garde comme avant.
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

// ─── Sécurité : la base entière ─────────────────────────────────

/// Les routes qui lisent ou remplacent la base entière. Les corps sont
/// valides pour la désérialisation : la garde, pas le corps, doit décider.
fn routes_de_la_base_entiere() -> Vec<(Method, &'static str, Option<&'static str>)> {
    vec![
        (Method::GET, "/api/v1/system/database/export", None),
        (Method::POST, "/api/v1/system/database/import", None),
        (
            Method::POST,
            "/api/v1/system/backups/encrypt",
            Some(r#"{"password":"x"}"#),
        ),
        (
            Method::POST,
            "/api/v1/system/backups/tune_backup_inexistant.db/restore",
            None,
        ),
    ]
}

/// Comme [`appel`], avec l'adresse RÉELLE du pair (`ConnectInfo`) et des
/// en-têtes supplémentaires.
async fn appel_depuis(
    state: &AppState,
    pair: std::net::SocketAddr,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
    entetes: &[(&str, &str)],
) -> (StatusCode, String) {
    let app: Router = tune_server::routes::router(state.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    for (k, v) in entetes {
        req = req.header(*k, *v);
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

fn pair(ip: &str) -> std::net::SocketAddr {
    std::net::SocketAddr::new(ip.parse().unwrap(), 50000)
}

/// Sans authentification : refus depuis le réseau local (et sans adresse
/// connue), même si un en-tête prétend le contraire.
#[tokio::test]
async fn sans_authentification_la_base_entiere_est_refusee_depuis_le_reseau() {
    let state = new_state();
    for (m, chemin, corps) in routes_de_la_base_entiere() {
        for ip in [
            "192.168.1.50",
            "10.0.0.7",
            "fd00::7",
            "::ffff:192.168.1.50",
            "203.0.113.5",
        ] {
            let (st, rep) = appel_depuis(
                &state,
                pair(ip),
                m.clone(),
                chemin,
                None,
                corps,
                &[("x-forwarded-for", "127.0.0.1"), ("x-real-ip", "127.0.0.1")],
            )
            .await;
            assert_eq!(
                st,
                StatusCode::FORBIDDEN,
                "{m} {chemin} depuis {ip} : {rep}"
            );
            assert!(
                rep.contains("auth_required"),
                "{m} {chemin} depuis {ip} : {rep}"
            );
        }
        // Sans `ConnectInfo` : l'adresse est inconnue, donc refusée.
        let (st, rep) = appel(&state, m.clone(), chemin, None, corps).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} sans pair : {rep}");
    }
}

/// Sans authentification : la machine elle-même passe la garde ; une requête
/// rejouée par le relais (vue de 127.0.0.1) non.
#[tokio::test]
async fn sans_authentification_la_base_entiere_reste_ouverte_depuis_cette_machine() {
    let state = new_state();
    for (m, chemin, corps) in routes_de_la_base_entiere() {
        for ip in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            let (_, rep) =
                appel_depuis(&state, pair(ip), m.clone(), chemin, None, corps, &[]).await;
            assert!(
                !rep.contains("auth_required"),
                "{m} {chemin} depuis {ip} : {rep}"
            );
        }
        let (st, rep) = appel_depuis(
            &state,
            pair("127.0.0.1"),
            m.clone(),
            chemin,
            None,
            corps,
            &[(tune_core::cloud::ENTETE_RELAIS, "1")],
        )
        .await;
        // Refusée par la garde du relais ou par celle de la base (403 tous
        // les deux) : jamais servie comme une requête locale.
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} relayée : {rep}");
    }
}

/// Authentification active : l'adresse ne compte plus, le rôle oui. Depuis
/// le réseau local, l'administrateur passe ; un compte standard est refusé,
/// un appel sans jeton aussi.
#[tokio::test]
async fn avec_authentification_la_base_entiere_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let admin = tok("admin", 1);
    let standard = tok("user", 2);
    for (m, chemin, corps) in routes_de_la_base_entiere() {
        let lan = pair("192.168.1.50");
        let (st, rep) =
            appel_depuis(&state, lan, m.clone(), chemin, Some(&admin), corps, &[]).await;
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
        assert!(!rep.contains("auth_required"), "{m} {chemin} : {rep}");
        assert!(!rep.contains("admin role required"), "{m} {chemin} : {rep}");
        let (st, rep) =
            appel_depuis(&state, lan, m.clone(), chemin, Some(&standard), corps, &[]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} : {rep}");
        assert!(rep.contains("admin role required"), "{m} {chemin} : {rep}");
        let (st, rep) = appel_depuis(&state, lan, m.clone(), chemin, None, corps, &[]).await;
        assert!(
            st == StatusCode::UNAUTHORIZED || st == StatusCode::FORBIDDEN,
            "{m} {chemin} sans jeton : {st} {rep}"
        );
    }
}

/// Le refus dit QUOI FAIRE, dans la langue de l'interface, avec sa clé : le
/// client web affiche `message` quand `message_key` est présent.
#[tokio::test]
async fn le_refus_de_la_base_entiere_dit_quoi_faire_dans_la_langue_de_l_interface() {
    let state = new_state();
    let lan = pair("192.168.1.50");
    for (langue, attendu) in [
        ("fr-FR,fr;q=0.9", "ouvrez Tune sur la machine du serveur"),
        ("en-US", "open Tune on the server machine itself"),
        ("", "open Tune on the server machine itself"),
    ] {
        let entetes: Vec<(&str, &str)> = if langue.is_empty() {
            vec![]
        } else {
            vec![("accept-language", langue)]
        };
        let (st, rep) = appel_depuis(
            &state,
            lan,
            Method::GET,
            "/api/v1/system/database/export",
            None,
            None,
            &entetes,
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
        let v: serde_json::Value = serde_json::from_str(&rep).unwrap();
        assert_eq!(v["error"], "auth_required", "{rep}");
        assert_eq!(v["message_key"], "api.database.localOnly", "{rep}");
        assert!(
            v["message"]
                .as_str()
                .is_some_and(|m| m.contains(attendu) && m.contains("http://localhost:8888")),
            "{langue:?} : {rep}"
        );
    }
}
