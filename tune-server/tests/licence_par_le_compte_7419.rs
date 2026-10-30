//! #7419 (alec_eiffel, fil 2530) — « J'ai installé Tune sur une autre machine.
//! Je me suis identifié dans Tune. Ma clé de licence n'est pas renseignée
//! automatiquement, je dois aller la chercher sur mon espace. »
//!
//! Le rapport joint au fil montre `license_account_premium_updated
//! account_premium=true` : le serveur ÉTAIT Premium par le compte, sans clé.
//! Le site déduit le Premium de la licence liée au courriel du compte
//! (`/api/v1/user` → `premium`), et aucune clé n'est à saisir. Mais
//! `/cloud/license/status` ne disait pas d'où venait ce Premium : le panneau
//! Licence montrait un champ de clé vide et invitait à « activer une
//! licence ». Il publie désormais `premium_source` : `account`, `key` ou
//! `null`.
//!
//! Le faux mozaiklabs.fr est un **vrai** serveur axum sur une socket locale
//! éphémère, comme `sso_modules_du_compte.rs` : jamais mozaiklabs.fr, jamais
//! une vraie clé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const AUTHORIZE: &str = "/api/v1/cloud/sso/authorize";
const STATUS: &str = "/api/v1/cloud/license/status";

/// Faux mozaiklabs.fr : l'échange de jeton PKCE et le profil du compte, dont
/// le drapeau `premium` est celui que le site déduit de la licence liée au
/// courriel.
async fn faux_mozaiklabs(premium: bool) -> String {
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            axum::routing::post(|| async {
                axum::Json(json!({
                    "access_token": "jeton-acces-7419",
                    "refresh_token": "jeton-rafraichissement-7419",
                    "expires_in": 2_592_000,
                }))
            }),
        )
        .route(
            "/api/v1/user",
            axum::routing::get(move || async move {
                axum::Json(json!({
                    "id": 7419,
                    "email": "nouvelle-machine@example.fr",
                    "display_name": "Nouvelle machine",
                    "is_admin": false,
                    "avatar_url": null,
                    "premium": premium,
                    "license_tier": if premium { "premium" } else { "free" },
                    "license_expires_at": null,
                    "qobuz_proxy_first": false,
                    "modules": [],
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

/// Base de travail jetable, à un chemin unique par test.
fn scratch_db() -> (tune_core::test_scratch::ScratchDir, std::path::PathBuf) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i7419");
    let db = dir.join("library.db");
    (dir, db)
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
        "/api/v1/cloud/sso/callback?code=code-7419&state={csrf}"
    ))
    .header("host", "127.0.0.1:8888")
    .body(Body::empty())
    .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// LE CAS DU FIL : une machine neuve, aucune clé saisie, la seule connexion
/// au compte. Le serveur est Premium, et il dit que c'est par le compte.
#[tokio::test]
async fn sur_une_nouvelle_machine_la_connexion_au_compte_suffit() {
    let base = faux_mozaiklabs(true).await;
    let (app, state, _garde) = serveur_froid(&base);

    let (_, avant) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(avant["tier"], "free", "état de départ : {avant}");
    assert!(
        avant["premium_source"].is_null(),
        "état de départ : {avant}"
    );

    assert_eq!(
        se_connecter(&app, &state).await,
        StatusCode::TEMPORARY_REDIRECT,
        "la connexion doit se terminer par la redirection vers l'interface"
    );

    let (code, corps) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        corps["tier"], "premium",
        "le Premium du compte n'est pas appliqué : {corps}"
    );
    assert!(
        corps["license_key"].is_null(),
        "aucune clé n'a été saisie sur cette machine : {corps}"
    );
    assert_eq!(
        corps["premium_source"], "account",
        "#7419 : le statut ne dit pas que le Premium vient du compte — le panneau \
         Licence invite alors à recopier une clé inutile : {corps}"
    );
}

/// Contre-partie : un compte GRATUIT ne devient pas Premium par la connexion,
/// et aucune source n'est inventée.
#[tokio::test]
async fn un_compte_gratuit_ne_donne_aucune_source_de_premium() {
    let base = faux_mozaiklabs(false).await;
    let (app, state, _garde) = serveur_froid(&base);
    se_connecter(&app, &state).await;

    let (_, corps) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(corps["tier"], "free", "statut : {corps}");
    assert!(corps["premium_source"].is_null(), "statut : {corps}");
}
