//! Première configuration : cas complémentaires (hôte, origine, version
//! HTTP, adresses IPv4 dans IPv6, comptes). Les cas qui posent des variables
//! d'environnement sont dans `premiere_configuration_environnement.rs`.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, Version, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::auth::ouvrir_la_premiere_configuration;
use tune_server::state::AppState;

/// Machine ordinaire, quel que soit l'environnement de l'épreuve. Même
/// valeur partout dans ce binaire.
fn hors_conteneur() {
    // SAFETY: même valeur dans toutes les épreuves de ce binaire.
    unsafe { std::env::set_var("TUNE_TEST_HORS_CONTENEUR", "1") };
}

fn installation_neuve() -> AppState {
    hors_conteneur();
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    assert!(ouvrir_la_premiere_configuration(&state));
    state
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

async fn creer_brut(
    state: &AppState,
    adresse: &str,
    version: Version,
    entetes: &[(&str, &str)],
    type_contenu: &str,
    corps: &str,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/auth/activation")
        .version(version)
        .header(header::CONTENT_TYPE, type_contenu);
    for (n, v) in entetes {
        req = req.header(*n, *v);
    }
    let resp = app
        .oneshot(req.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let st = resp.status();
    let o = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&o)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&o).to_string()));
    (st, v)
}

async fn creer(state: &AppState, adresse: &str, entetes: &[(&str, &str)]) -> (StatusCode, Value) {
    creer_brut(
        state,
        adresse,
        Version::HTTP_11,
        entetes,
        "application/json",
        r#"{"password":"intrus-1234"}"#,
    )
    .await
}

// --- Forge de « local » : refus attendus ---------------------------------

#[tokio::test]
async fn rebinding_dns_hote_public_refuse() {
    // Page de evil.example rebindée sur l'IP du serveur : le navigateur
    // envoie Host = evil.example et une origine « même origine ».
    let state = installation_neuve();
    let (st, v) = creer(
        &state,
        "192.168.1.50:50000",
        &[
            ("host", "evil.example:8888"),
            ("origin", "http://evil.example:8888"),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert!(
        st == StatusCode::MISDIRECTED_REQUEST || st == StatusCode::FORBIDDEN,
        "{st} {v}"
    );
    assert!(!active(&state));
}

#[tokio::test]
async fn csrf_text_plain_autre_site_refuse() {
    let state = installation_neuve();
    let (st, v) = creer_brut(
        &state,
        "192.168.1.50:50000",
        Version::HTTP_11,
        &[
            ("host", "192.168.1.20:8888"),
            ("origin", "http://evil.example"),
        ],
        "text/plain",
        r#"{"password":"intrus-1234"}"#,
    )
    .await;
    assert!(st.is_client_error(), "{st} {v}");
    assert!(!active(&state));
}

#[tokio::test]
async fn csrf_origine_null_refuse() {
    let state = installation_neuve();
    let (st, v) = creer(
        &state,
        "192.168.1.50:50000",
        &[("host", "192.168.1.20:8888"), ("origin", "null")],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
}

#[tokio::test]
async fn same_site_autre_hote_du_lan_refuse() {
    // Page servie par un autre appareil du LAN (tune.lan vs nas.lan).
    let state = installation_neuve();
    let (st, v) = creer(
        &state,
        "192.168.1.50:50000",
        &[
            ("host", "tune.lan:8888"),
            ("origin", "http://nas.lan"),
            ("sec-fetch-site", "same-site"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
}

#[tokio::test]
async fn http_1_0_refuse() {
    let state = installation_neuve();
    let (st, v) = creer_brut(
        &state,
        "192.168.1.50:50000",
        Version::HTTP_10,
        &[],
        "application/json",
        r#"{"password":"intrus-1234"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
}

#[tokio::test]
async fn ipv4_mappee_hors_lan_refusee() {
    for a in ["[::ffff:203.0.113.5]:50000", "[::ffff:100.64.1.2]:50000"] {
        let state = installation_neuve();
        let (st, v) = creer(&state, a, &[]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{a} {v}");
        assert!(!active(&state));
    }
}

#[tokio::test]
async fn ipv4_mappee_et_ipv6_locales_acceptees() {
    // Comportement voulu : LAN en double pile, ULA, lien local.
    for a in [
        "[::ffff:192.168.1.50]:50000",
        "[fd00::5]:50000",
        "[fe80::5]:50000",
    ] {
        let state = installation_neuve();
        let (st, v) = creer(&state, a, &[]).await;
        assert_eq!(st, StatusCode::OK, "{a} {v}");
    }
}

// --- La voie de première configuration ne supprime aucun compte.

#[tokio::test]
async fn premiere_configuration_supprime_des_comptes() {
    let state = installation_neuve();
    let h = tune_server::auth::hash_password("autre-1234").unwrap();
    {
        use tune_core::db::backend::ToSqlValue;
        state
            .backend
            .execute(
                "INSERT INTO profiles (username, is_admin, password_hash_v2) VALUES ('autre', 0, ?)",
                &[&h as &dyn ToSqlValue],
            )
            .unwrap();
    }
    let id: i64 = state
        .backend
        .query_one("SELECT id FROM profiles WHERE username = 'autre'", &[])
        .unwrap()
        .unwrap()
        .first()
        .and_then(|v| v.as_i64())
        .unwrap();
    let corps = format!(r#"{{"password":"intrus-1234","delete_accounts":[{id}]}}"#);
    let (st, v) = creer_brut(
        &state,
        "192.168.1.50:50000",
        Version::HTTP_11,
        &[],
        "application/json",
        &corps,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(!active(&state));
    assert!(
        state
            .backend
            .query_one("SELECT id FROM profiles WHERE username = 'autre'", &[])
            .unwrap()
            .is_some(),
        "le compte reste"
    );
}
