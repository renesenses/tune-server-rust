//! Reconnexion au compte mozaiklabs après une attente imposée par le cloud.
//!
//! Trois défauts du même aller-retour de connexion :
//!
//! 1. une attente mémorisée sur la relecture du profil (`GET /api/v1/user`)
//!    n'était regardée qu'APRÈS l'échange du code : chaque essai faisait
//!    émettre un jeton par mozaiklabs, aussitôt jeté ;
//! 2. le refus arrivait au navigateur en JSON brut, sans explication ni
//!    chemin de retour vers Tune ;
//! 3. un délai plus long que la fenêtre de cette route était mémorisé, et
//!    bloquait les essais suivants sans même les laisser partir.
//!
//! Le faux mozaiklabs.fr est un vrai serveur axum sur une socket locale
//! éphémère, qui compte les appels qu'il reçoit.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt;
use tune_core::cloud::rate_limit::{self, CloudScope};
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const AUTHORIZE: &str = "/api/v1/cloud/sso/authorize";

struct Compteurs {
    jetons: Arc<AtomicUsize>,
    profils: Arc<AtomicUsize>,
}

/// Faux mozaiklabs.fr. `/api/v1/user` répond `profil_statut`, avec
/// `Retry-After: retry_after` quand il est donné.
async fn faux_mozaiklabs(
    profil_statut: u16,
    retry_after: Option<&'static str>,
) -> (String, Compteurs) {
    let jetons = Arc::new(AtomicUsize::new(0));
    let profils = Arc::new(AtomicUsize::new(0));
    let (j, p) = (jetons.clone(), profils.clone());
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            axum::routing::post(move || {
                let j = j.clone();
                async move {
                    j.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({
                        "access_token": "jeton-acces",
                        "refresh_token": "jeton-rafraichissement",
                        "expires_in": 2_592_000,
                    }))
                }
            }),
        )
        .route(
            "/api/v1/user",
            axum::routing::get(move || {
                let p = p.clone();
                async move {
                    p.fetch_add(1, Ordering::SeqCst);
                    let mut reponse = axum::response::Response::new(Body::from(
                        json!({
                            "id": 7,
                            "email": "compte@example.fr",
                            "display_name": "Compte",
                            "is_admin": false,
                            "avatar_url": null,
                            "premium": false,
                            "modules": [],
                        })
                        .to_string(),
                    ));
                    *reponse.status_mut() = StatusCode::from_u16(profil_statut).unwrap();
                    reponse.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    if let Some(secs) = retry_after {
                        reponse
                            .headers_mut()
                            .insert(header::RETRY_AFTER, HeaderValue::from_static(secs));
                    }
                    reponse
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), Compteurs { jetons, profils })
}

fn serveur(base_url: &str) -> (axum::Router, AppState, tune_core::test_scratch::ScratchDir) {
    let dir = tune_core::test_scratch::scratch_dir("tune-sso-attente");
    let db = dir.join("library.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base_url).unwrap();
    settings.set("mozaik_client_id", "tune-server").unwrap();
    (tune_server::routes::router(state.clone()), state, dir)
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

/// Pose une attente comme le ferait un 429 de `/api/v1/user`.
fn poser_attente(state: &AppState, secondes: &'static str) {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::RETRY_AFTER,
        reqwest::header::HeaderValue::from_static(secondes),
    );
    rate_limit::defer_from_headers(&reglages(state), CloudScope::UserProfile, &h)
        .expect("l'attente doit être posée");
}

struct Page {
    statut: StatusCode,
    type_contenu: String,
    retry_after: Option<String>,
    corps: String,
}

async fn lire(app: &axum::Router, uri: &str) -> Page {
    let req = Request::get(uri)
        .header("host", "127.0.0.1:8888")
        .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let type_contenu = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let retry_after = resp
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    Page {
        statut,
        type_contenu,
        retry_after,
        corps: String::from_utf8_lossy(&octets).into_owned(),
    }
}

/// `/sso/authorize` puis `/sso/callback`, avec le `state` émis par le serveur.
async fn aller_retour(app: &axum::Router, state: &AppState) -> Page {
    let autorisation = lire(app, AUTHORIZE).await;
    assert_eq!(
        autorisation.statut,
        StatusCode::TEMPORARY_REDIRECT,
        "l'autorisation doit rediriger : {}",
        autorisation.corps
    );
    let pending = reglages(state)
        .get("mozaik_pkce_pending")
        .ok()
        .flatten()
        .expect("aucune session PKCE en attente");
    let pkce: serde_json::Value = serde_json::from_str(&pending).unwrap();
    let csrf = pkce["state"].as_str().unwrap().to_string();
    lire(
        app,
        &format!("/api/v1/cloud/sso/callback?code=code-test&state={csrf}"),
    )
    .await
}

fn assert_page_attente(page: &Page, minutes: u64) {
    assert_eq!(page.statut, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        page.type_contenu.starts_with("text/html"),
        "le navigateur doit recevoir une page, pas du JSON : {} / {}",
        page.type_contenu,
        page.corps
    );
    assert!(
        page.corps.contains(&format!("Try again in {minutes} min.")),
        "la page doit donner le temps restant ({minutes} min) : {}",
        page.corps
    );
    assert!(
        page.corps.contains(r#"<a href="/">Back to Tune</a>"#),
        "la page doit ramener à Tune : {}",
        page.corps
    );
    assert!(
        page.corps.contains("The account was not linked"),
        "la page doit dire que le compte n'est pas lié : {}",
        page.corps
    );
    assert!(
        page.retry_after.is_some(),
        "l'en-tête Retry-After doit rester"
    );
}

/// Une attente active : l'autorisation ne redirige PAS vers mozaiklabs, et le
/// navigateur reçoit une page qui donne le temps restant.
#[tokio::test]
async fn une_attente_active_est_dite_avant_de_rediriger() {
    let (base, compteurs) = faux_mozaiklabs(200, None).await;
    let (app, state, _garde) = serveur(&base);
    poser_attente(&state, "45");

    let page = lire(&app, AUTHORIZE).await;

    assert_page_attente(&page, 1);
    assert_eq!(
        reglages(&state).get("mozaik_pkce_pending").ok().flatten(),
        None,
        "aucune session PKCE ne doit être ouverte pendant l'attente"
    );
    assert_eq!(compteurs.jetons.load(Ordering::SeqCst), 0);
}

/// Une attente posée ENTRE l'autorisation et le retour : le code n'est pas
/// échangé, donc aucun jeton n'est émis pour rien.
#[tokio::test]
async fn une_attente_posee_pendant_l_aller_retour_n_echange_pas_le_code() {
    let (base, compteurs) = faux_mozaiklabs(200, None).await;
    let (app, state, _garde) = serveur(&base);

    let autorisation = lire(&app, AUTHORIZE).await;
    assert_eq!(autorisation.statut, StatusCode::TEMPORARY_REDIRECT);
    poser_attente(&state, "30");
    let pending = reglages(&state)
        .get("mozaik_pkce_pending")
        .ok()
        .flatten()
        .unwrap();
    let pkce: serde_json::Value = serde_json::from_str(&pending).unwrap();
    let page = lire(
        &app,
        &format!(
            "/api/v1/cloud/sso/callback?code=code-test&state={}",
            pkce["state"].as_str().unwrap()
        ),
    )
    .await;

    assert_page_attente(&page, 1);
    assert_eq!(
        compteurs.jetons.load(Ordering::SeqCst),
        0,
        "le code a été échangé alors que la relecture du profil était retenue"
    );
}

/// Un délai expiré ne bloque plus : la connexion aboutit.
#[tokio::test]
async fn un_delai_expire_ne_bloque_plus() {
    let (base, compteurs) = faux_mozaiklabs(200, None).await;
    let (app, state, _garde) = serveur(&base);
    reglages(&state)
        .set("cloud_rate_limit_until:user_profile", "1")
        .unwrap();

    let page = aller_retour(&app, &state).await;

    assert_eq!(
        page.statut,
        StatusCode::TEMPORARY_REDIRECT,
        "la connexion doit aboutir : {}",
        page.corps
    );
    assert_eq!(compteurs.profils.load(Ordering::SeqCst), 1);
}

/// Le profil refusé en 429 : une page HTML avec le temps restant, plus de JSON.
#[tokio::test]
async fn un_429_du_profil_rend_une_page_et_pas_du_json() {
    let (base, _) = faux_mozaiklabs(429, Some("30")).await;
    let (app, state, _garde) = serveur(&base);

    let page = aller_retour(&app, &state).await;

    assert_page_attente(&page, 1);
    assert!(
        !page.corps.contains("cloud_rate_limited"),
        "le JSON brut ne doit plus arriver au navigateur : {}",
        page.corps
    );
}

/// Le cas vécu : 1 433 s annoncées par `/api/v1/user`, dont la fenêtre est
/// d'une minute. Ce délai est DIT (24 min), mais pas mémorisé : l'essai
/// suivant part.
#[tokio::test]
async fn un_delai_hors_fenetre_est_dit_mais_ne_bloque_pas_l_essai_suivant() {
    let (base, compteurs) = faux_mozaiklabs(429, Some("1433")).await;
    let (app, state, _garde) = serveur(&base);

    let page = aller_retour(&app, &state).await;
    assert_page_attente(&page, 24);
    assert!(
        rate_limit::active(&reglages(&state), CloudScope::UserProfile).is_none(),
        "un délai plus long que la fenêtre de la route ne doit pas être mémorisé"
    );

    let suivante = lire(&app, AUTHORIZE).await;
    assert_eq!(
        suivante.statut,
        StatusCode::TEMPORARY_REDIRECT,
        "l'essai suivant doit pouvoir partir : {}",
        suivante.corps
    );
    assert_eq!(compteurs.profils.load(Ordering::SeqCst), 1);
}

/// Les autres refus du retour sont aussi des pages.
#[tokio::test]
async fn un_refus_ordinaire_du_retour_est_une_page() {
    let (base, _) = faux_mozaiklabs(200, None).await;
    let (app, _state, _garde) = serveur(&base);

    let page = lire(&app, "/api/v1/cloud/sso/callback?error=access_denied").await;

    assert_eq!(page.statut, StatusCode::BAD_REQUEST);
    assert!(
        page.type_contenu.starts_with("text/html"),
        "{}",
        page.type_contenu
    );
    assert!(page.corps.contains("access_denied"), "{}", page.corps);
    assert!(
        page.corps.contains(r#"<a href="/">Back to Tune</a>"#),
        "{}",
        page.corps
    );
}
