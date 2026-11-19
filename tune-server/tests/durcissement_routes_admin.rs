//! Routes d'administration : un témoin par route.
//!
//! - authentification active : un compte standard reçoit 403, un appel sans
//!   jeton 401, l'administrateur passe la garde ;
//! - authentification coupée : ce qui n'a rien à faire hors administration
//!   est refusé explicitement (`auth_required`), le reste passe comme avant.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
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

/// Cette machine : l'adresse de bouclage, comme le client web servi en local.
const CETTE_MACHINE: &str = "127.0.0.1:50000";
/// Un autre hôte du réseau local.
const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";

async fn appel(
    state: &AppState,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
) -> (StatusCode, String) {
    let (st, _, rep) = appel_depuis(state, CETTE_MACHINE, methode, chemin, jeton, corps, &[]).await;
    (st, rep)
}

/// Même appel, mais depuis une adresse de connexion donnée (`ConnectInfo`,
/// comme `into_make_service_with_connect_info` la pose en production) et avec
/// des en-têtes supplémentaires. Rend aussi les en-têtes de la réponse.
#[allow(clippy::too_many_arguments)]
async fn appel_depuis(
    state: &AppState,
    adresse: &str,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
    entetes: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
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
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
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
        // A5 : `sso/disconnect` n'est plus réservé à l'administrateur — un compte
        // standard délie SON profil (voir
        // `un_compte_standard_ne_delie_que_son_propre_profil`).
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
        // Sécurité : actions qui engagent la machine.
        (Method::POST, "/api/v1/appliance/reboot", None),
        (Method::POST, "/api/v1/appliance/shutdown", None),
        (
            Method::POST,
            "/api/v1/appliance/install-to-disk",
            Some(r#"{"device":"sdz","confirm":"EFFACER"}"#),
        ),
    ]
}

/// Sécurité : les actions qui engagent la machine, ou qui installent du code.
/// Les deux routes de la boutique ne figurent pas dans
/// `routes_d_administration` : passé la garde, elles appellent la boutique
/// en ligne, ce qu'un test ne fait pas.
fn actions_machine() -> Vec<(Method, &'static str, Option<&'static str>)> {
    vec![
        (Method::POST, "/api/v1/appliance/reboot", None),
        (Method::POST, "/api/v1/appliance/shutdown", None),
        (
            Method::POST,
            "/api/v1/appliance/install-to-disk",
            Some(r#"{"device":"sdz","confirm":"EFFACER"}"#),
        ),
        (
            Method::POST,
            "/api/v1/marketplace/plugins/exemple/install",
            None,
        ),
        (
            Method::POST,
            "/api/v1/marketplace/plugins/exemple/update",
            None,
        ),
        // Seconde route d'installation depuis la boutique.
        (Method::POST, "/api/v1/cloud/plugins/exemple/install", None),
        // Éteindre à l'heure dite = `/appliance/shutdown` par une porte de côté.
        (
            Method::PUT,
            "/api/v1/system/settings/scheduled-stop",
            Some(r#"{"enabled":true,"time":"23:00","action":"poweroff"}"#),
        ),
    ]
}

/// Les routes qui, passée la garde, ne partent pas en ligne : témoins « la
/// garde passe » sans appel à la boutique.
/// La garde « cette machine » est-elle passée ? Son refus porte
/// `auth_required` ; le gestionnaire de l'arrêt programmé, lui, refuse
/// d'éteindre un HÔTE ordinaire sans authentification active (#8909) avec son
/// propre message — garde passée, refus du gestionnaire.
fn garde_passee(rep: &str) -> bool {
    !rep.contains("auth_required")
        || rep.contains(tune_server::extinction_hote::REFUS_SANS_AUTHENTIFICATION)
}

fn actions_machine_hors_ligne() -> Vec<(Method, &'static str, Option<&'static str>)> {
    actions_machine()
        .into_iter()
        .filter(|(_, chemin, _)| !chemin.contains("/plugins/"))
        .collect()
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
    // Le retour passe la couche d'authentification (le navigateur revient de
    // mozaiklabs.fr) : c'est le gestionnaire qui répond, par une PAGE.
    // Sans la session qui a lancé la liaison, il refuse ; avec elle
    // (administrateur ou compte standard, cookie ou jeton), il traite le
    // retour — ici le refus du fournisseur.
    let retour = "/api/v1/cloud/sso/callback?error=access_denied";
    let (st, rep) = appel(&state, Method::GET, retour, None, None).await;
    assert!(
        rep.contains("<!doctype html>"),
        "page du gestionnaire : {st} {rep}"
    );
    assert!(
        !rep.contains("OAuth error"),
        "sans session, le retour n'est pas traité : {rep}"
    );
    for jeton in [tok("admin", 1), tok("user", 2)] {
        let (st, rep) = appel(&state, Method::GET, retour, Some(&jeton), None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "avec la session : {rep}");
        assert!(rep.contains("OAuth error: access_denied"), "{rep}");
    }
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

/// Sécurité : authentification coupée, une action machine n'est acceptée que
/// depuis cette machine — jamais depuis un autre hôte du réseau, jamais par le
/// relais (vu de 127.0.0.1 mais émis ailleurs).
#[tokio::test]
async fn sans_authentification_les_actions_machine_viennent_de_cette_machine() {
    let state = new_state();
    for (m, chemin, corps) in actions_machine() {
        let (st, _, rep) = appel_depuis(
            &state,
            AUTRE_HOTE_DU_LAN,
            m.clone(),
            chemin,
            None,
            corps,
            &[],
        )
        .await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{m} {chemin} depuis le LAN : {rep}"
        );
        assert!(rep.contains("auth_required"), "{m} {chemin} : {rep}");

        let (st, _, rep) = appel_depuis(
            &state,
            CETTE_MACHINE,
            m.clone(),
            chemin,
            None,
            corps,
            &[("x-tune-relais", "1")],
        )
        .await;
        // Le relais refuse d'abord ce qui n'est pas relayable
        // (`not_available_remotely`) ; une route relayable tomberait sur la
        // garde (`auth_required`). Dans les deux cas : 403, jamais l'action.
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} relayé : {rep}");
        assert!(
            rep.contains("auth_required") || rep.contains("not_available_remotely"),
            "{m} {chemin} relayé : {rep}"
        );
    }
    // Témoin : depuis cette machine, la garde passe (la réponse vient ensuite
    // du gestionnaire : 404 hors appliance, 400 poweroff hors Tune OS).
    for (m, chemin, corps) in actions_machine_hors_ligne() {
        let (st, rep) = appel(&state, m.clone(), chemin, None, corps).await;
        assert!(garde_passee(&rep), "{m} {chemin} : {rep}");
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
    }
}

/// « Cette machine » en IPv6 : `::1` et la forme IPv4 projetée du bouclage
/// passent ; la forme projetée d'un hôte du LAN est refusée.
#[tokio::test]
async fn sans_authentification_le_bouclage_ipv6_est_cette_machine() {
    let state = new_state();
    for adresse in ["[::1]:50000", "[::ffff:127.0.0.1]:50000"] {
        for (m, chemin, corps) in actions_machine_hors_ligne() {
            let (st, _, rep) =
                appel_depuis(&state, adresse, m.clone(), chemin, None, corps, &[]).await;
            assert!(garde_passee(&rep), "{adresse} {m} {chemin} : {rep}");
            assert_ne!(
                st,
                StatusCode::UNAUTHORIZED,
                "{adresse} {m} {chemin} : {rep}"
            );
        }
    }
    for (m, chemin, corps) in actions_machine() {
        let (st, _, rep) = appel_depuis(
            &state,
            "[::ffff:192.168.1.50]:50000",
            m.clone(),
            chemin,
            None,
            corps,
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} : {rep}");
        assert!(rep.contains("auth_required"), "{m} {chemin} : {rep}");
        assert!(rep.contains("\"caller\":\"::ffff:192.168.1.50\""), "{rep}");
    }
}

/// Sans adresse de connexion, la réponse est NON — et le refus le dit
/// (`caller: unknown`). Un `X-Forwarded-For: 127.0.0.1` posé par un hôte du
/// LAN ne change rien : seul `ConnectInfo` compte.
#[tokio::test]
async fn sans_adresse_de_connexion_ou_avec_un_x_forwarded_for_la_garde_refuse() {
    let state = new_state();
    for (m, chemin, corps) in actions_machine() {
        // Routeur nu, sans MockConnectInfo.
        let app: Router = tune_server::routes::router(state.clone());
        let mut req = Request::builder().method(m.clone()).uri(chemin);
        let body = match corps {
            Some(c) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(c.to_string())
            }
            None => Body::empty(),
        };
        let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
        let st = resp.status();
        let rep = String::from_utf8_lossy(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .to_string();
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{m} {chemin} sans ConnectInfo : {rep}"
        );
        assert!(
            rep.contains("\"caller\":\"unknown\""),
            "{m} {chemin} : {rep}"
        );

        let (st, _, rep) = appel_depuis(
            &state,
            AUTRE_HOTE_DU_LAN,
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
            "{m} {chemin} X-Forwarded-For : {rep}"
        );
        assert!(rep.contains("\"caller\":\"192.168.1.50\""), "{rep}");
    }
}

/// Sécurité : le réglage brut `scheduled_stop` ne contourne pas la garde de
/// l'arrêt programmé — c'est une clé protégée, refusée sans administrateur
/// effectif, d'où que vienne l'appel.
#[tokio::test]
async fn le_reglage_brut_de_l_arret_programme_est_protege() {
    let state = new_state();
    let corps = r#"{"scheduled_stop":{"enabled":true,"time":"23:00","action":"poweroff"}}"#;
    for adresse in [AUTRE_HOTE_DU_LAN, CETTE_MACHINE] {
        let (st, _, rep) = appel_depuis(
            &state,
            adresse,
            Method::PATCH,
            "/api/v1/system/config",
            None,
            Some(corps),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{adresse} : {rep}");
        assert!(rep.contains("protected_settings"), "{adresse} : {rep}");
        assert!(rep.contains("scheduled_stop"), "{adresse} : {rep}");
    }
    // Et rien n'a été écrit.
    let (st, rep) = appel(
        &state,
        Method::GET,
        "/api/v1/system/settings/scheduled-stop",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    // `poweroff_available` figure toujours dans la réponse : c'est le réglage
    // lui-même qui doit être resté vide.
    assert!(rep.contains("\"scheduled_stop\":null"), "{rep}");
}

/// Webhooks (décision du 15/11) : la machine elle-même n'est pas une
/// destination ; le réseau local l'est.
#[tokio::test]
async fn un_webhook_ne_vise_jamais_la_machine_mais_le_lan_passe() {
    let state = new_state();
    state.license.set_account_premium(true, None).await;
    let creer = |url: String| {
        let state = state.clone();
        async move {
            let corps = format!(r#"{{"url":"{url}","events":["track.started"]}}"#);
            appel(
                &state,
                Method::POST,
                "/api/v1/developer/webhooks",
                None,
                Some(&corps),
            )
            .await
        }
    };
    let (st, rep) = creer("http://192.168.1.10:8123/api/webhook/tune".into()).await;
    assert_eq!(st, StatusCode::CREATED, "LAN : {rep}");
    let (st, rep) = creer("http://10.0.0.5:5678/webhook/tune".into()).await;
    assert_eq!(st, StatusCode::CREATED, "LAN (10/8) : {rep}");

    let mut refusees = vec![
        "http://127.0.0.1:8888/api/v1/appliance/reboot".to_string(),
        "http://[::1]:8888/api/v1/appliance/reboot".to_string(),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        "http://0.0.0.0:8888/x".to_string(),
        "http://localhost:8888/x".to_string(),
    ];
    // L'adresse d'une interface de cette machine, s'il y en a une hors bouclage.
    if let Some(locale) = tune_core::discovery::adresses_de_la_machine()
        .into_iter()
        .find(|ip| !ip.is_loopback() && ip.is_ipv4())
    {
        refusees.push(format!("http://{locale}:8888/api/v1/appliance/reboot"));
    }
    for url in refusees {
        let (st, rep) = creer(url.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{url} : {rep}");
        assert!(rep.contains("webhook_destination_refused"), "{url} : {rep}");
    }
}

/// Le nom d'un greffon de la boutique est un slug : rien ne sort du dossier
/// des greffons, même depuis cette machine. Le `%2F` est décodé par axum.
#[tokio::test]
async fn l_installation_depuis_le_nuage_refuse_un_nom_qui_n_est_pas_un_slug() {
    let state = new_state();
    let trop_long = format!("/api/v1/cloud/plugins/{}/install", "a".repeat(65));
    for chemin in [
        "/api/v1/cloud/plugins/..%2F..%2Fx/install",
        "/api/v1/cloud/plugins/Evil/install",
        "/api/v1/cloud/plugins/a%2Fb/install",
        "/api/v1/cloud/plugins/-tiret/install",
        "/api/v1/cloud/plugins/a.b/install",
        trop_long.as_str(),
    ] {
        let (st, rep) = appel(&state, Method::POST, chemin, None, None).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{chemin} : {rep}");
        assert!(rep.contains("invalid_plugin_name"), "{chemin} : {rep}");
    }
}

/// Sécurité : authentification active, les actions machine exigent
/// l'administrateur, d'où que vienne l'appel — y compris de cette machine.
#[tokio::test]
async fn sous_authentification_les_actions_machine_exigent_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let standard = tok("user", 2);
    for (m, chemin, corps) in actions_machine() {
        let (st, rep) = appel(&state, m.clone(), chemin, None, corps).await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "{m} {chemin} sans jeton : {rep}"
        );
        let (st, rep) = appel(&state, m.clone(), chemin, Some(&standard), corps).await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{m} {chemin} compte standard : {rep}"
        );
        assert!(rep.contains("admin role required"), "{m} {chemin} : {rep}");
    }
}

const INSCRIPTION: &str = "/api/v1/auth/register";
const CORPS_INSCRIPTION: &str = r#"{"username":"invite","password":"motdepasse-solide"}"#;

/// Sécurité : authentification active, l'inscription est fermée. Sans jeton
/// 401, compte standard 403 ; seul l'administrateur crée un compte, et il
/// garde sa propre session (aucun cookie posé).
#[tokio::test]
async fn sous_authentification_seul_l_administrateur_cree_un_compte() {
    let state = new_state();
    enable_auth(&state);
    let (st, _, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        INSCRIPTION,
        None,
        Some(CORPS_INSCRIPTION),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "sans jeton : {rep}");

    let (st, _, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        INSCRIPTION,
        Some(&tok("user", 2)),
        Some(CORPS_INSCRIPTION),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "compte standard : {rep}");
    assert!(rep.contains("admin role required"), "{rep}");

    let (st, entetes, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        INSCRIPTION,
        Some(&tok("admin", 1)),
        Some(CORPS_INSCRIPTION),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "administrateur : {rep}");
    assert!(rep.contains("\"username\":\"invite\""), "{rep}");
    assert!(
        entetes.get(header::SET_COOKIE).is_none(),
        "l'administrateur garde sa session : {entetes:?}"
    );
}

/// Témoin : authentification coupée, le premier compte de l'onboarding reste
/// possible DEPUIS CETTE MACHINE, avec sa session. Depuis un autre hôte, un
/// compte créé par un tiers survivrait à l'activation : refusé.
#[tokio::test]
async fn sans_authentification_le_premier_compte_s_inscrit_toujours() {
    let state = new_state();
    let (st, _, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        INSCRIPTION,
        None,
        Some(CORPS_INSCRIPTION),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "depuis le LAN : {rep}");
    assert!(rep.contains("not_local"), "{rep}");
    let (st, _, rep) = appel_depuis(
        &state,
        CETTE_MACHINE,
        Method::POST,
        INSCRIPTION,
        None,
        Some(CORPS_INSCRIPTION),
        &[("x-forwarded-for", "192.168.1.50")],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "derrière un mandataire : {rep}");
    let (st, entetes, rep) = appel_depuis(
        &state,
        CETTE_MACHINE,
        Method::POST,
        INSCRIPTION,
        None,
        Some(CORPS_INSCRIPTION),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{rep}");
    assert!(rep.contains("\"token\""), "{rep}");
    assert!(
        entetes
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("tune_session=")),
        "{entetes:?}"
    );
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

/// Comme [`appel_depuis`], avec l'adresse du pair en `SocketAddr`.
#[allow(clippy::too_many_arguments)]
async fn appel_depuis_pair(
    state: &AppState,
    pair: SocketAddr,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
    entetes: &[(&str, &str)],
) -> (StatusCode, String) {
    let (st, _, rep) = appel_depuis(
        state,
        &pair.to_string(),
        methode,
        chemin,
        jeton,
        corps,
        entetes,
    )
    .await;
    (st, rep)
}

/// Routeur nu, sans `ConnectInfo` : l'adresse du pair est inconnue.
async fn appel_sans_pair(
    state: &AppState,
    methode: Method,
    chemin: &str,
    corps: Option<&str>,
) -> (StatusCode, String) {
    let app: Router = tune_server::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(chemin);
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

fn pair(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 50000)
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
            let (st, rep) = appel_depuis_pair(
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
        let (st, rep) = appel_sans_pair(&state, m.clone(), chemin, corps).await;
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
                appel_depuis_pair(&state, pair(ip), m.clone(), chemin, None, corps, &[]).await;
            assert!(
                !rep.contains("auth_required"),
                "{m} {chemin} depuis {ip} : {rep}"
            );
        }
        let (st, rep) = appel_depuis_pair(
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
            appel_depuis_pair(&state, lan, m.clone(), chemin, Some(&admin), corps, &[]).await;
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
        assert!(!rep.contains("auth_required"), "{m} {chemin} : {rep}");
        assert!(!rep.contains("admin role required"), "{m} {chemin} : {rep}");
        let (st, rep) =
            appel_depuis_pair(&state, lan, m.clone(), chemin, Some(&standard), corps, &[]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} : {rep}");
        assert!(rep.contains("admin role required"), "{m} {chemin} : {rep}");
        let (st, rep) = appel_depuis_pair(&state, lan, m.clone(), chemin, None, corps, &[]).await;
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
        let (st, rep) = appel_depuis_pair(
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

// ─── Intégrations : configuration réservée, la machine n'est pas une destination

/// Sans authentification, l'adresse de Home Assistant ne se règle que depuis
/// cette machine ; l'adresse de la machine elle-même est refusée, une adresse
/// du réseau local admise.
#[tokio::test]
async fn l_adresse_de_home_assistant_ne_vise_jamais_la_machine() {
    let state = new_state();
    let chemin = "/api/v1/homeassistant/config";
    let (st, _, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        chemin,
        None,
        Some(r#"{"ha_url":"http://192.168.1.20:8123","ha_token":"x"}"#),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "depuis le LAN : {rep}");
    assert!(rep.contains("auth_required"), "{rep}");

    for url in [
        "http://127.0.0.1:8888",
        "http://localhost:8888",
        "http://[::1]:8888",
        "http://0.0.0.0:8888",
        "http://169.254.169.254",
    ] {
        let corps = format!(r#"{{"ha_url":"{url}","ha_token":"x"}}"#);
        let (st, rep) = appel(&state, Method::POST, chemin, None, Some(&corps)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{url} : {rep}");
        assert!(rep.contains("ha_destination_refused"), "{url} : {rep}");
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(reglages.get("ha_url").ok().flatten(), None);

    let (st, rep) = appel(
        &state,
        Method::POST,
        chemin,
        None,
        Some(r#"{"ha_url":"http://192.168.1.20:8123/","ha_token":"x"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    assert_eq!(
        reglages.get("ha_url").ok().flatten().as_deref(),
        Some("http://192.168.1.20:8123")
    );
}

/// Une adresse de Home Assistant déjà enregistrée vers la machine n'est
/// jamais appelée : chaque appel sortant est jugé avant de partir.
#[tokio::test]
async fn un_appel_home_assistant_vers_la_machine_ne_part_pas() {
    let state = new_state();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set("ha_url", "http://127.0.0.1:8888/api/v1/appliance/shutdown?")
        .unwrap();
    reglages.set("ha_token", "x").unwrap();
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/homeassistant/entities/z/call",
        None,
        Some(r#"{"domain":"a","service":"b","service_data":{}}"#),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
    assert!(rep.contains("ha_destination_refused"), "{rep}");
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/homeassistant/automations/trigger",
        None,
        Some(r#"{"entity_id":"automation.x"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
    assert!(rep.contains("ha_destination_refused"), "{rep}");
}

/// Hue : sans authentification, la configuration du pont ne se règle que
/// depuis cette machine, et seulement sous la forme `hôte[:port]` + jeton.
#[tokio::test]
async fn la_configuration_hue_est_reservee_et_nue() {
    let state = new_state();
    let chemin = "/api/v1/hue/config";
    let corps = r#"{"hue_bridge_ip":"192.168.1.30","hue_username":"abc-DEF_1"}"#;
    let (st, _, rep) = appel_depuis(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        chemin,
        None,
        Some(corps),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "depuis le LAN : {rep}");
    assert!(rep.contains("auth_required"), "{rep}");
    for mauvais in [
        r#"{"hue_bridge_ip":"127.0.0.1:8888/api/v1/appliance/shutdown?"}"#,
        r#"{"hue_bridge_ip":"192.168.1.30#"}"#,
        r#"{"hue_bridge_ip":"x@127.0.0.1"}"#,
        r#"{"hue_username":"../../system/restart?"}"#,
    ] {
        let (st, rep) = appel(&state, Method::POST, chemin, None, Some(mauvais)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{mauvais} : {rep}");
    }
    let (st, rep) = appel(&state, Method::POST, chemin, None, Some(corps)).await;
    assert_eq!(st, StatusCode::OK, "{rep}");
}

/// L'adresse du serveur Snapcast ne s'écrit plus par l'écriture générique des
/// réglages sans administrateur.
#[tokio::test]
async fn l_adresse_snapcast_est_un_reglage_protege() {
    let state = new_state();
    let (st, rep) = appel(
        &state,
        Method::PATCH,
        "/api/v1/system/config",
        None,
        Some(r#"{"snapcast_host":"192.168.1.40:1705"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
    assert!(rep.contains("snapcast_host"), "{rep}");
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(reglages.get("snapcast_host").ok().flatten(), None);
}

/// Actions qui engagent la machine ou le code exécuté et qui n'avaient que
/// `RequireAdmin` (ou aucune garde) : arrêt du serveur, greffons (installation,
/// mise à jour, activation, désactivation, retrait), retrait d'un thème,
/// greffons audio natifs, ouverture de l'accès à distance. Les noms
/// n'existent pas : passé la garde, chaque gestionnaire refuse sans rien
/// toucher.
fn actions_machine_gardees_a_leur_tour() -> Vec<(Method, &'static str, Option<&'static str>)> {
    vec![
        (Method::POST, "/api/v1/system/stop", None),
        (
            Method::POST,
            "/api/v1/plugins/greffon-inexistant-gardes/install",
            Some("{}"),
        ),
        (
            Method::POST,
            "/api/v1/plugins/greffon-inexistant-gardes/update",
            None,
        ),
        (
            Method::POST,
            "/api/v1/plugins/greffon-inexistant-gardes/enable",
            None,
        ),
        (
            Method::POST,
            "/api/v1/plugins/greffon-inexistant-gardes/disable",
            None,
        ),
        (
            Method::DELETE,
            "/api/v1/plugins/greffon-inexistant-gardes",
            None,
        ),
        (
            Method::POST,
            "/api/v1/marketplace/plugins/greffon-inexistant-gardes/uninstall",
            None,
        ),
        (
            Method::DELETE,
            "/api/v1/skins/theme-inexistant-gardes",
            None,
        ),
        (
            Method::POST,
            "/api/v1/audio-plugins/greffon-inexistant-gardes/install",
            None,
        ),
        (
            Method::POST,
            "/api/v1/audio-plugins/greffon-inexistant-gardes/install-from-catalog",
            None,
        ),
        (
            Method::POST,
            "/api/v1/audio-plugins/greffon-inexistant-gardes/rollback",
            None,
        ),
        (
            Method::POST,
            "/api/v1/audio-plugins/greffon-inexistant-gardes/uninstall",
            Some("{}"),
        ),
        (Method::POST, "/api/v1/cloud/bridge/enable", None),
        // Décision du 18/11 : restaurations / imports de configuration, pont,
        // canal de mise à jour.
        (Method::POST, "/api/v1/cloud/bridge/disable", None),
        (Method::POST, "/api/v1/cloud/bridge/rotate-token", None),
        (Method::POST, "/api/v1/system/config/import", Some("{}")),
        (
            Method::POST,
            "/api/v1/system/config-backup/import",
            Some("{}"),
        ),
        (
            Method::POST,
            "/api/v1/system/config-backup/cloud/restore",
            Some("{}"),
        ),
        (
            Method::PUT,
            "/api/v1/system/update/channel",
            Some(r#"{"channel":"stable"}"#),
        ),
        // Le lien d'accès porte le jeton du pont.
        (Method::GET, "/api/v1/cloud/bridge/access-link", None),
    ]
}

/// Passé la garde, l'arrêt quitte le processus : jamais exécuté sous test.
fn sans_l_arret(
    routes: Vec<(Method, &'static str, Option<&'static str>)>,
) -> Vec<(Method, &'static str, Option<&'static str>)> {
    routes
        .into_iter()
        .filter(|(_, chemin, _)| !chemin.ends_with("/system/stop"))
        .collect()
}

#[tokio::test]
async fn sans_authentification_arret_et_greffons_refuses_depuis_le_reseau_local() {
    let state = new_state();
    for (m, chemin, corps) in actions_machine_gardees_a_leur_tour() {
        let (st, _, rep) = appel_depuis(
            &state,
            AUTRE_HOTE_DU_LAN,
            m.clone(),
            chemin,
            None,
            corps,
            &[],
        )
        .await;
        assert_eq!(
            st,
            StatusCode::FORBIDDEN,
            "{m} {chemin} depuis le LAN : {rep}"
        );
        assert!(rep.contains("auth_required"), "{m} {chemin} : {rep}");
        assert!(rep.contains("not_local"), "{m} {chemin} : {rep}");

        // Relayé : vu de 127.0.0.1, émis ailleurs. Jamais local.
        let (st, _, rep) = appel_depuis(
            &state,
            CETTE_MACHINE,
            m.clone(),
            chemin,
            None,
            corps,
            &[("x-tune-relais", "1")],
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{m} {chemin} relayé : {rep}");
    }
    // Rien n'a été écrit par les appels refusés.
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    for cle in [
        "plugin_greffon-inexistant-gardes_installed",
        "plugin_greffon-inexistant-gardes_enabled",
    ] {
        assert_eq!(reglages.get(cle).ok().flatten(), None, "{cle}");
    }
}

#[tokio::test]
async fn sans_authentification_arret_et_greffons_acceptes_depuis_cette_machine() {
    let state = new_state();
    for adresse in [CETTE_MACHINE, "[::1]:50000"] {
        for (m, chemin, corps) in sans_l_arret(actions_machine_gardees_a_leur_tour()) {
            let (st, _, rep) =
                appel_depuis(&state, adresse, m.clone(), chemin, None, corps, &[]).await;
            assert!(
                !rep.contains("auth_required"),
                "{adresse} {m} {chemin} : {rep}"
            );
            assert_ne!(
                st,
                StatusCode::UNAUTHORIZED,
                "{adresse} {m} {chemin} : {rep}"
            );
        }
    }
}

#[tokio::test]
async fn avec_authentification_arret_et_greffons_exigent_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let standard = tok("user", 2);
    for (m, chemin, corps) in actions_machine_gardees_a_leur_tour() {
        // Depuis le réseau local comme depuis cette machine : le rôle décide.
        for adresse in [AUTRE_HOTE_DU_LAN, CETTE_MACHINE] {
            let (st, _, rep) = appel_depuis(
                &state,
                adresse,
                m.clone(),
                chemin,
                Some(&standard),
                corps,
                &[],
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{adresse} {m} {chemin} : {rep}");
            assert!(rep.contains("admin role required"), "{m} {chemin} : {rep}");
            let (st, _, rep) =
                appel_depuis(&state, adresse, m.clone(), chemin, None, corps, &[]).await;
            assert_eq!(
                st,
                StatusCode::UNAUTHORIZED,
                "{adresse} {m} {chemin} sans jeton : {rep}"
            );
        }
    }
    // L'administrateur passe la garde, y compris depuis un autre hôte du LAN.
    let admin = tok("admin", 1);
    for (m, chemin, corps) in sans_l_arret(actions_machine_gardees_a_leur_tour()) {
        let (st, _, rep) = appel_depuis(
            &state,
            AUTRE_HOTE_DU_LAN,
            m.clone(),
            chemin,
            Some(&admin),
            corps,
            &[],
        )
        .await;
        assert_ne!(st, StatusCode::UNAUTHORIZED, "{m} {chemin} : {rep}");
        assert!(!rep.contains("admin role required"), "{m} {chemin} : {rep}");
        assert!(!rep.contains("auth_required"), "{m} {chemin} : {rep}");
    }
}

/// Les drapeaux des greffons ne s'écrivent plus par l'écriture générique des
/// réglages sans administrateur : ce serait la porte de côté des routes
/// d'installation.
#[tokio::test]
async fn les_drapeaux_des_greffons_sont_des_reglages_proteges() {
    let state = new_state();
    for corps in [
        r#"{"plugin_bandcamp_installed":"true"}"#,
        r#"{"plugin_bandcamp_enabled":"true"}"#,
        r#"{"plugin_bandcamp_hidden":"false"}"#,
        r#"{"plugins":"[]"}"#,
        // Greffons Premium retenus : la licence retrouvée les rallumerait.
        r#"{"premium_audio_plugins_withheld_equalizer":"installed,enabled"}"#,
        r#"{"premium_audio_plugins_migration_v1":""}"#,
    ] {
        let (st, rep) = appel(
            &state,
            Method::PATCH,
            "/api/v1/system/config",
            None,
            Some(corps),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{corps} : {rep}");
        assert!(rep.contains("protected_settings"), "{corps} : {rep}");
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        reglages.get("plugin_bandcamp_installed").ok().flatten(),
        None
    );
}

/// Derrière un mandataire inverse local, tout appel arrive de 127.0.0.1 : la
/// présence d'un en-tête de mandataire retire la preuve « cette machine ».
#[tokio::test]
async fn derriere_un_mandataire_le_bouclage_ne_vaut_plus_cette_machine() {
    let state = new_state();
    for (entete, valeur) in [
        ("x-forwarded-for", "192.168.1.50"),
        ("forwarded", "for=192.168.1.50"),
        ("x-real-ip", "192.168.1.50"),
        ("via", "1.1 caddy"),
        ("cf-connecting-ip", "203.0.113.9"),
        ("x-forwarded-host", "tune.example.org"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-port", "443"),
        ("x-client-ip", "192.168.1.50"),
        ("fastly-client-ip", "203.0.113.9"),
        ("tailscale-user-login", "intrus@example.org"),
        // Un `Host` qui ne nomme pas cette machine : relais TCP, proxy DSM…
        ("host", "nas.local:8888"),
        ("host", "192.168.1.10:8888"),
    ] {
        for (m, chemin, corps) in [
            (Method::POST, "/api/v1/system/stop", None),
            (
                Method::POST,
                "/api/v1/plugins/greffon-inexistant-gardes/install",
                Some("{}"),
            ),
            (
                Method::POST,
                "/api/v1/auth/activation",
                Some(r#"{"password":"tiers-1234"}"#),
            ),
        ] {
            let (st, _, rep) = appel_depuis(
                &state,
                CETTE_MACHINE,
                m.clone(),
                chemin,
                None,
                corps,
                &[(entete, valeur)],
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{entete} {m} {chemin} : {rep}");
        }
    }
    assert!(!tune_server::auth::authentification_active(&state));
    // Témoin : sans en-tête, la même machine passe la garde.
    let (_, _, rep) = appel_depuis(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/plugins/greffon-inexistant-gardes/install",
        None,
        Some("{}"),
        &[],
    )
    .await;
    assert!(!rep.contains("auth_required"), "{rep}");
}

#[test]
fn un_mandataire_declare_retire_la_preuve_du_bouclage() {
    use tune_server::auth::mandataire_declare;
    assert!(!mandataire_declare(None, None));
    assert!(!mandataire_declare(Some(""), Some(" , ")));
    assert!(mandataire_declare(Some("https://tune.example.org"), None));
    assert!(mandataire_declare(None, Some("tune.example.org")));
}

/// Changer l'apparence active pour tous : administrateur quand
/// l'authentification est active.
#[tokio::test]
async fn le_theme_actif_exige_l_administrateur() {
    let state = new_state();
    enable_auth(&state);
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/skins/active",
        Some(&tok("user", 2)),
        Some(r#"{"skin_id":"theme-inexistant"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{rep}");
    assert!(rep.contains("admin role required"), "{rep}");
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/skins/active",
        Some(&tok("admin", 1)),
        Some(r#"{"skin_id":"theme-inexistant"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{rep}");
}

/// Un `Host` de bouclage (avec ou sans port, IPv4, IPv6, `localhost`) reste
/// « cette machine ».
#[tokio::test]
async fn un_host_de_bouclage_reste_cette_machine() {
    let state = new_state();
    for hote in [
        "localhost:8888",
        "127.0.0.1:8888",
        "[::1]:8888",
        "localhost",
        "LOCALHOST.",
    ] {
        let (st, _, rep) = appel_depuis(
            &state,
            CETTE_MACHINE,
            Method::POST,
            "/api/v1/plugins/greffon-inexistant-gardes/install",
            None,
            Some("{}"),
            &[("host", hote)],
        )
        .await;
        assert!(!rep.contains("auth_required"), "{hote} : {st} {rep}");
    }
}

#[test]
fn les_noms_d_hote_de_bouclage() {
    use tune_server::auth::hote_de_bouclage;
    for oui in [
        "localhost",
        "localhost:8888",
        "LOCALHOST",
        "LocalHost:8888",
        "localhost.",
        "localhost.:8888",
        "127.0.0.1",
        "127.0.0.1:80",
        "127.42.0.7:8888",
        "[::1]",
        "[::1]:8888",
        "[0:0:0:0:0:0:0:1]:8888",
    ] {
        assert!(hote_de_bouclage(oui), "{oui}");
    }
    for non in [
        "nas.local",
        "nas.local:8888",
        "192.168.1.10:8888",
        "[fd00::4]:8888",
        "localhost.example.org",
        "localhost.evil",
        "localhost.evil:8888",
        "app.localhost",
        "127.0.0.1.nip.io",
        // Formes abrégées ou octales : refusées (le navigateur les normalise).
        "127.1",
        "0177.0.0.1",
        "user@localhost",
        "user@localhost:8888",
        "[::1",
        "",
    ] {
        assert!(!hote_de_bouclage(non), "{non}");
    }
}

/// HTTP/1.0 n'est jamais « cette machine » : c'est l'empreinte de nginx par
/// défaut (`proxy_http_version 1.0`, `Host` réécrit, sans `X-Forwarded-*`).
/// HTTP/1.1 sans `Host` reste accepté.
#[tokio::test]
async fn http_1_0_n_est_jamais_cette_machine() {
    let state = new_state();
    let pair: SocketAddr = CETTE_MACHINE.parse().unwrap();
    let envoyer = |version: axum::http::Version, hote: Option<&'static str>| {
        let state = state.clone();
        async move {
            let app: Router = tune_server::routes::router(state).layer(MockConnectInfo(pair));
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/api/v1/plugins/greffon-inexistant-gardes/install")
                .version(version)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(h) = hote {
                req = req.header(header::HOST, h);
            }
            let resp = app
                .oneshot(req.body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            let st = resp.status();
            let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (st, String::from_utf8_lossy(&octets).to_string())
        }
    };
    for hote in [None, Some("127.0.0.1"), Some("localhost:8888")] {
        let (st, rep) = envoyer(axum::http::Version::HTTP_10, hote).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "HTTP/1.0 {hote:?} : {rep}");
        assert!(rep.contains("not_local"), "{rep}");
    }
    let (_, rep) = envoyer(axum::http::Version::HTTP_11, None).await;
    assert!(!rep.contains("auth_required"), "HTTP/1.1 sans Host : {rep}");
    let (_, rep) = envoyer(axum::http::Version::HTTP_11, Some("localhost:8888")).await;
    assert!(
        !rep.contains("auth_required"),
        "HTTP/1.1 Host localhost : {rep}"
    );
}

/// Un compte standard qui appelle `sso/disconnect` ne délie que SON profil :
/// jamais le compte relié au serveur, jamais un autre profil.
#[tokio::test]
async fn un_compte_standard_ne_delie_que_son_propre_profil() {
    let state = new_state();
    enable_auth(&state);
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    // Le serveur est relié ; les profils 2 et 3 aussi, chacun au sien.
    reglages
        .set("mozaik_access_token", "JETON-DU-SERVEUR-FAUX")
        .unwrap();
    reglages
        .set("mozaik_user", r#"{"email":"proprio@example.org"}"#)
        .unwrap();
    reglages
        .set("mozaik_compte_profil_2", r#"{"email":"deux@example.org"}"#)
        .unwrap();
    reglages
        .set("mozaik_compte_profil_3", r#"{"email":"trois@example.org"}"#)
        .unwrap();
    let (st, rep) = appel(
        &state,
        Method::POST,
        "/api/v1/cloud/sso/disconnect",
        Some(&tok("user", 2)),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rep}");
    assert!(rep.contains("\"scope\":\"profile\""), "{rep}");
    assert_eq!(
        reglages.get("mozaik_compte_profil_2").unwrap(),
        None,
        "son profil est délié"
    );
    assert!(
        reglages.get("mozaik_compte_profil_3").unwrap().is_some(),
        "l'autre profil reste relié"
    );
    assert_eq!(
        reglages.get("mozaik_access_token").unwrap().as_deref(),
        Some("JETON-DU-SERVEUR-FAUX"),
        "le serveur reste relié"
    );
    assert!(
        reglages.get("mozaik_user").unwrap().is_some(),
        "le compte du serveur reste"
    );
    // Sans jeton : 401, rien ne bouge.
    let (st, _) = appel(
        &state,
        Method::POST,
        "/api/v1/cloud/sso/disconnect",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(reglages.get("mozaik_compte_profil_3").unwrap().is_some());
}
