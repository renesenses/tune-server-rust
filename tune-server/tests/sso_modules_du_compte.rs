//! #2138 — après la connexion mozaiklabs, le serveur doit appliquer **tout** ce
//! que le compte porte, pas seulement le palier premium.
//!
//! Le ticket vient d'un testeur premium à qui l'application annonçait « compte
//! gratuit » juste après s'être connecté. Sur l'application iPadOS, la cause est
//! côté écran (dépôt `tune-server-universal`). Mais le même chemin de connexion
//! existe ici, et il souffre d'un défaut de la même famille : `sso_callback`
//! applique le palier (`set_account_premium`) et l'ordre Qobuz
//! (`set_qobuz_proxy_first`), mais **pas** les modules payants
//! (`set_modules`) — alors que `refresh_account_premium`
//! (`tune-server/src/background.rs`) applique bien les quatre.
//!
//! `set_modules` n'avait qu'un seul appelant dans tout le dépôt : le battement
//! de fond, cadencé à `HEARTBEAT_INTERVAL` = 3600 s. Conséquence mesurable :
//! pendant une heure après la connexion, un compte qui possède le module payant
//! « diretta » s'entend répondre `module_not_owned` / `purchase_module` — soit,
//! mot pour mot, « ce compte ne possède pas ce module » à quelqu'un qui vient de
//! le payer. C'est le refus construit par `premium_guard::ModuleRefusal`, et
//! c'est la charge utile que le client affiche.
//!
//! Le faux mozaiklabs.fr est un **vrai** serveur axum sur une socket locale
//! éphémère : jamais mozaiklabs.fr, et jamais une socket coupée à la main (un
//! RST rend les tests intermittents sur ce dépôt).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::premium_guard::ModuleRefusal;
use tune_server::state::AppState;

const MODULE: &str = "diretta";
const AUTHORIZE: &str = "/api/v1/cloud/sso/authorize";
const STATUS: &str = "/api/v1/cloud/license/status";

/// Base de travail jetable, à un chemin unique par test.
fn scratch_db() -> (tune_core::test_scratch::ScratchDir, std::path::PathBuf) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i2138");
    let db = dir.join("library.db");
    (dir, db)
}

/// Faux mozaiklabs.fr : l'échange de jeton PKCE et le profil de compte.
///
/// Le profil rendu est celui d'un abonné premium qui possède **en plus** le
/// module payant « diretta » (SKU distinct du palier, cf. `CloudUser::modules`).
async fn faux_mozaiklabs() -> String {
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "access_token": "jeton-acces-2138",
                    "refresh_token": "jeton-rafraichissement-2138",
                    "expires_in": 2_592_000,
                }))
            }),
        )
        .route(
            "/api/v1/user",
            axum::routing::get(|| async {
                axum::Json(json!({
                    "id": 42,
                    "email": "patatorz@example.fr",
                    "display_name": "Patatorz",
                    "is_admin": false,
                    "avatar_url": null,
                    "premium": true,
                    "license_tier": "premium",
                    "license_expires_at": "2099-01-01T00:00:00Z",
                    "qobuz_proxy_first": false,
                    "modules": [MODULE],
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://{addr}")
}

/// Un serveur Tune **froid** : base neuve, aucune clé, aucun compte lié, le
/// cloud redirigé vers le bouchon.
fn serveur_froid(base_url: &str) -> (axum::Router, AppState, tune_core::test_scratch::ScratchDir) {
    let (base, db_path) = scratch_db();
    let state = AppState::new(db_path.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base_url).unwrap();
    settings.set("mozaik_client_id", "tune-server").unwrap();
    assert_eq!(
        settings.get("mozaik_access_token").ok().flatten(),
        None,
        "l'état de départ doit être froid : aucun compte lié"
    );
    (tune_server::routes::router(state.clone()), state, base)
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Le vrai aller-retour de connexion : `/sso/authorize` frappe la session PKCE,
/// puis `/sso/callback` la consomme avec le `state` que le serveur a lui-même
/// émis. Rien n'est fabriqué à la main.
async fn se_connecter(app: &axum::Router, state: &AppState) -> StatusCode {
    let req = Request::get(AUTHORIZE)
        .header("host", "127.0.0.1:8888")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TEMPORARY_REDIRECT,
        "l'autorisation doit rediriger vers le fournisseur"
    );

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let pending = settings
        .get("mozaik_pkce_pending")
        .ok()
        .flatten()
        .expect("aucune session PKCE en attente");
    let pkce: Value = serde_json::from_str(&pending).unwrap();
    let csrf = pkce["state"].as_str().expect("state PKCE absent");

    let req = Request::get(format!(
        "/api/v1/cloud/sso/callback?code=code-2138&state={csrf}"
    ))
    .header("host", "127.0.0.1:8888")
    .body(Body::empty())
    .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Le cœur de #2138, côté serveur : après la connexion, la charge utile rendue
/// au client porte le niveau **premium** — et le module payant que le compte
/// possède n'est plus refusé.
#[tokio::test]
async fn apres_la_connexion_le_compte_porte_premium_et_ses_modules() {
    let base = faux_mozaiklabs().await;
    let (app, state, _garde) = serveur_froid(&base);

    let statut = se_connecter(&app, &state).await;
    assert_eq!(
        statut,
        StatusCode::TEMPORARY_REDIRECT,
        "la connexion doit se terminer par la redirection vers l'interface"
    );

    // Témoin — vrai avant comme après le correctif : le palier voyage bien.
    let (code, corps) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        corps["tier"], "premium",
        "le palier du compte n'est pas parvenu au client : {corps}"
    );

    // Le défaut : le droit de module reste vide, et la charge utile rendue au
    // client accuse le testeur de ne pas posséder ce qu'il vient de payer.
    let possede = state.license.has_module(MODULE).await;
    let refus = ModuleRefusal::evaluate(possede, true);
    assert!(
        refus.is_none(),
        "après la connexion, le module possédé par le compte est encore refusé : {}",
        refus.unwrap().to_json(MODULE)
    );
    assert_eq!(
        state.license.modules().await,
        vec![MODULE.to_string()],
        "les modules du compte ne sont pas appliqués par la connexion"
    );
}

/// Contre-partie, qui doit rester vraie : un compte premium qui ne possède
/// AUCUN module ne s'en voit accorder aucun. Le correctif propage ce que le
/// cloud dit, il n'invente rien.
#[tokio::test]
async fn un_compte_sans_module_n_en_recoit_aucun() {
    let app_cloud = axum::Router::new()
        .route(
            "/oauth/token",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "access_token": "jeton-acces-2138-bis",
                    "expires_in": 2_592_000,
                }))
            }),
        )
        .route(
            "/api/v1/user",
            axum::routing::get(|| async {
                axum::Json(json!({
                    "id": 43,
                    "email": "sans-module@example.fr",
                    "display_name": "Sans module",
                    "is_admin": false,
                    "avatar_url": null,
                    "premium": true,
                    "license_expires_at": "2099-01-01T00:00:00Z",
                    "modules": [],
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app_cloud).await.ok();
    });

    let (app, state, _garde) = serveur_froid(&format!("http://{addr}"));
    se_connecter(&app, &state).await;

    let (_, corps) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(corps["tier"], "premium", "statut : {corps}");
    assert!(
        state.license.modules().await.is_empty(),
        "un module a été accordé sans que le compte le porte"
    );
    assert_eq!(
        ModuleRefusal::evaluate(state.license.has_module(MODULE).await, true),
        Some(ModuleRefusal::NotOwned),
        "le refus doit rester nommé pour un module réellement non possédé"
    );
}
