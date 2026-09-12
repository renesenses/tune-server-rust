//! Security regression tests for the auth layer.
//!
//! These lock down the P0 reported in the 2026-08 security audit: with auth
//! *enabled*, the privileged `/auth/` endpoints (token minting, API-key
//! rotation, mutating the auth config) were reachable unauthenticated because
//! the middleware allowlisted anything containing `/auth/`. A single anonymous
//! request could mint a year-long JWT, rotate the API key, or turn auth back
//! off — i.e. enabling auth protected nothing.
//!
//! Also covered: the passwordless `default` admin must not be claimable from
//! the network once auth is on (only loopback / first-run), and first-run
//! setup must stay open when auth is disabled.

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use std::net::SocketAddr;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const REMOTE: &str = "203.0.113.7:44444";
const LOCAL: &str = "127.0.0.1:5555";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings.set("jwt_secret", "test-jwt-secret").unwrap();
}

/// Build the full app with a simulated peer address so the `ConnectInfo`
/// extractor on `/auth/login` resolves under `oneshot`.
fn app(state: &AppState, peer: &str) -> Router {
    let peer: SocketAddr = peer.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(MockConnectInfo(peer))
}

async fn post_json(
    app: &Router,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
    let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn get_status(app: &Router, path: &str, token: Option<&str>) -> StatusCode {
    let mut req = Request::get(path);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// The core of the P0: none of the privileged auth endpoints may act on an
/// anonymous request once auth is enabled.
#[tokio::test]
async fn privileged_auth_endpoints_reject_anonymous_when_auth_enabled() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    // Turning auth back off — the most damaging of the bunch.
    let (st, _) = post_json(
        &app,
        "/api/v1/auth/config",
        None,
        r#"{"auth_enabled":false}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "anonymous must not disable auth"
    );

    // Minting a long-lived JWT.
    let (st, _) = post_json(&app, "/api/v1/auth/token", None, "{}").await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "anonymous must not mint a token"
    );

    // Rotating / reading the API key.
    let (st, _) = post_json(&app, "/api/v1/auth/api-key", None, "{}").await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "anonymous must not rotate the api key"
    );
    let st = get_status(&app, "/api/v1/auth/api-key", None).await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "anonymous must not read the api key"
    );

    // Auth is still enabled after the attempts.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        settings.get("auth_enabled").unwrap().as_deref(),
        Some("true")
    );
}

/// The genuinely-public handshakes must keep working when auth is enabled,
/// otherwise the login page can't function.
#[tokio::test]
async fn public_handshakes_still_reachable_when_auth_enabled() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    // Read-only status the login page reads before showing the form.
    let st = get_status(&app, "/api/v1/auth/config", None).await;
    assert_eq!(st, StatusCode::OK, "GET /auth/config must stay public");

    // Login must reach its handler (JSON invalid-credentials body proves it
    // ran; a middleware block would be a plain-text 401 instead).
    let (st, body) = post_json(
        &app,
        "/api/v1/auth/login",
        None,
        r#"{"username":"nope","password":"x"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(
        body.contains("invalid credentials"),
        "login handler should run, got: {body}"
    );
}

/// A network attacker must not seize the passwordless `default` admin account.
#[tokio::test]
async fn passwordless_default_rejected_from_remote() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    let (st, body) = post_json(
        &app,
        "/api/v1/auth/login",
        None,
        r#"{"username":"default","password":"whatever"}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "remote passwordless default login must be refused, got: {body}"
    );
}

/// The local operator can still claim the default admin on first login, and the
/// resulting admin token can then drive the privileged endpoints.
#[tokio::test]
async fn passwordless_default_allowed_from_loopback_then_admin_can_rotate() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, LOCAL);

    let (st, body) = post_json(
        &app,
        "/api/v1/auth/login",
        None,
        r#"{"username":"default","password":"firstpass"}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "loopback first-run login should succeed: {body}"
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["role"].as_str(), Some("admin"));
    let token = v["token"]
        .as_str()
        .expect("token in login response")
        .to_string();

    // That admin token unlocks the privileged endpoints.
    let (st, _) = post_json(&app, "/api/v1/auth/api-key", Some(&token), "{}").await;
    assert_eq!(
        st,
        StatusCode::OK,
        "admin should be able to generate an api key"
    );
}

/// With auth disabled (the default), first-run setup must stay open — the admin
/// UI enables auth from an unauthenticated, secret-less state.
#[tokio::test]
async fn auth_disabled_keeps_first_run_setup_open() {
    let state = new_state();
    let app = app(&state, REMOTE);

    let (st, _) = post_json(
        &app,
        "/api/v1/auth/config",
        None,
        r#"{"auth_enabled":true}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "first-run enable-auth must not be blocked"
    );
}

/// Brute-force throttling: after LOGIN_MAX_FAILURES bad attempts from one IP,
/// further attempts are rejected with 429. Uses a dedicated IP so it can't
/// interfere with (or be tripped by) the other tests' failed logins.
#[tokio::test]
async fn login_is_rate_limited_after_repeated_failures() {
    let state = new_state();
    let app = app(&state, "198.51.100.9:5000");
    for _ in 0..10 {
        let (st, _) = post_json(
            &app,
            "/api/v1/auth/login",
            None,
            r#"{"username":"nope","password":"bad"}"#,
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let (st, _) = post_json(
        &app,
        "/api/v1/auth/login",
        None,
        r#"{"username":"nope","password":"bad"}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::TOO_MANY_REQUESTS,
        "should lock out after 10 failures"
    );
}

/// Registration enforces a minimum password length (raised 4 -> 8).
#[tokio::test]
async fn register_rejects_short_password() {
    let state = new_state();
    let app = app(&state, LOCAL);
    let (st, body) = post_json(
        &app,
        "/api/v1/auth/register",
        None,
        r#"{"username":"newuser","password":"short7"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(body.contains("8 characters"), "got: {body}");
}

/// The session cookie carries `Secure` when the request came over HTTPS
/// (X-Forwarded-Proto), and omits it on plain HTTP so LAN use isn't broken.
#[tokio::test]
async fn session_cookie_secure_follows_forwarded_proto() {
    use axum::body::Body;
    use tower::ServiceExt;

    let mk = |proto: &str| {
        Request::post("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-forwarded-proto", proto)
            .body(Body::from(
                r#"{"username":"default","password":"whatever"}"#.to_string(),
            ))
            .unwrap()
    };

    let state = new_state();
    let resp = app(&state, LOCAL).oneshot(mk("https")).await.unwrap();
    let c = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(c.contains("Secure"), "https cookie must be Secure: {c}");

    let state = new_state();
    let resp = app(&state, LOCAL).oneshot(mk("http")).await.unwrap();
    let c = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(!c.contains("Secure"), "http cookie must not be Secure: {c}");
}

// ── /system/peer-info : la poignée de main entre serveurs ────────────────
//
// Un serveur Tune protégé était IMPOSSIBLE à ajouter comme pair : l'autre
// bout appelle `/system/peer-info` sans jeton (`fetch_peer_info`,
// routes/system/admin.rs) et n'a aucun moyen d'en obtenir un. La route est
// donc publique, délibérément — mais sa surface doit le rester.

/// Sans cette ouverture, la découverte entre pairs est morte dès que l'auth
/// est activée : l'appelant reçoit 401 et conclut « injoignable ».
#[tokio::test]
async fn peer_info_reste_joignable_sans_jeton_quand_l_auth_est_activee() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    assert_eq!(
        get_status(&app, "/api/v1/system/peer-info", None).await,
        StatusCode::OK,
        "un serveur protégé doit rester ajoutable comme pair"
    );
}

/// Le témoin de l'ouverture : ce qui sort ne doit rester QUE la carte de
/// visite. Si un champ s'ajoute au handler, ce test le dit — l'ouverture a
/// été accordée sur cette surface-là, pas sur une autre.
#[tokio::test]
async fn peer_info_anonyme_ne_publie_que_la_carte_de_visite() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/system/peer-info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let obj = v.as_object().expect("peer-info rend un objet");

    let mut champs: Vec<&str> = obj.keys().map(String::as_str).collect();
    champs.sort_unstable();
    assert_eq!(
        champs,
        vec!["name", "tracks", "version", "zones"],
        "la surface anonyme de peer-info a changé — l'ouverture ne vaut que \
         pour la carte de visite (nom, version, pistes, zones)"
    );
}

/// L'ouverture est bornée à GET : un POST homonyme futur ne doit pas en
/// hériter. `contains()` aurait donné cette ouverture aux deux.
#[tokio::test]
async fn peer_info_n_ouvre_pas_les_ecritures() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    let (st, _) = post_json(&app, "/api/v1/system/peer-info", None, "{}").await;
    assert_ne!(st, StatusCode::OK, "seul le GET de peer-info est public");
}

/// Anti-régression du voisinage : `/system/peers` (la liste que CE serveur a
/// découverte) reste fermée. Les deux noms se ressemblent, un `contains()`
/// mal écrit ouvrirait les deux.
#[tokio::test]
async fn peers_reste_ferme_meme_si_peer_info_est_ouvert() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    assert_eq!(
        get_status(&app, "/api/v1/system/peers", None).await,
        StatusCode::UNAUTHORIZED,
        "/system/peers n'est pas /system/peer-info"
    );
}

/// Anti-régression de l'arbitrage voisin : `/system/profile` (music_dirs,
/// IP LAN) doit rester fermé. C'est la limite que l'ouverture ne franchit pas.
#[tokio::test]
async fn profile_reste_ferme_meme_si_peer_info_est_ouvert() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, REMOTE);

    assert_eq!(
        get_status(&app, "/api/v1/system/profile", None).await,
        StatusCode::UNAUTHORIZED,
        "/system/profile expose music_dirs : il reste sous jeton"
    );
}
