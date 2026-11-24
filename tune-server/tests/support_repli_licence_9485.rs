//! #9485 (fil 3049) — la session SSO ouverte dans Tune n'est pas celle du
//! compte qui porte la licence Premium du serveur.
//!
//! Le site refuse alors le jeton SSO en `403 premium_required`, et l'écran
//! Support affichait « réservé à Tune Premium » alors que la clé de licence du
//! serveur, elle, est Premium. Comme le catalogue de greffons, le support
//! retente avec la clé de licence sur ce refus-là, et seulement celui-là.
//!
//! Un faux site, monté dans le test, refuse le jeton et accepte la clé ; il
//! note dans l'ordre l'identité présentée à chaque requête.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const TICKETS: &str = "/api/v1/support/tickets";

#[derive(Clone, Copy)]
enum Jeton {
    /// Le compte SSO n'a pas de licence : 403 premium_required.
    SansLicence,
    /// Le compte SSO est Premium : 200.
    Premium,
}

/// Faux site : rend la racine et le journal des identités présentées
/// (« jeton » ou « cle »), dans l'ordre.
async fn faux_site(jeton: Jeton) -> (String, Arc<Mutex<Vec<&'static str>>>) {
    let vus: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let journal = vus.clone();
    let app = Router::new().fallback(move |methode: Method, entetes: HeaderMap| {
        let journal = journal.clone();
        async move {
            let porteur = entetes
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("Bearer "));
            let cle = entetes.contains_key("X-License-Key");
            let succes = if methode == Method::POST {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let corps = json!({ "tickets": [{ "id": 7 }], "ticket": { "id": 7 } });
            if porteur {
                journal.lock().unwrap().push("jeton");
                return match jeton {
                    Jeton::Premium => (succes, axum::Json(corps)),
                    Jeton::SansLicence => (
                        StatusCode::FORBIDDEN,
                        axum::Json(json!({
                            "error": "premium_required",
                            "message": "Le support prioritaire est réservé à Tune Premium",
                        })),
                    ),
                };
            }
            if cle {
                journal.lock().unwrap().push("cle");
                return (succes, axum::Json(corps));
            }
            journal.lock().unwrap().push("rien");
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": "unauthenticated" })),
            )
        }
    });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://127.0.0.1:{port}"), vus)
}

fn app_vers(base: &str, licence: bool) -> Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("mozaik_base_url", base).unwrap();
    reglages
        .set("mozaik_access_token", "jeton-sso-9485")
        .unwrap();
    if licence {
        reglages.set("license_key", "TUNE-TEST-9485").unwrap();
        reglages
            .set("hardware_fingerprint", "empreinte-9485")
            .unwrap();
    }
    tune_server::routes::router(state).layer(axum::extract::connect_info::MockConnectInfo(
        std::net::SocketAddr::from(([127, 0, 0, 1], 50000)),
    ))
}

async fn appel(app: &Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn lister() -> Request<Body> {
    Request::get(TICKETS).body(Body::empty()).unwrap()
}

/// Le cas du fil 3049 : session SSO sans licence, licence du serveur valide
/// → les tickets se chargent, par la clé, après le refus du jeton.
#[tokio::test]
async fn session_sso_sans_licence_les_tickets_passent_par_la_cle() {
    let (base, vus) = faux_site(Jeton::SansLicence).await;
    let app = app_vers(&base, true);
    let (statut, corps) = appel(&app, lister()).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_ne!(corps["error"], "premium_required", "{corps}");
    assert_eq!(corps["tickets"][0]["id"], 7, "{corps}");
    assert_eq!(*vus.lock().unwrap(), vec!["jeton", "cle"]);
}

/// Même repli pour l'ouverture d'un ticket (chemin JSON) et le détail.
#[tokio::test]
async fn ouverture_et_detail_retentent_aussi_avec_la_cle() {
    let (base, vus) = faux_site(Jeton::SansLicence).await;
    let app = app_vers(&base, true);
    let (statut, corps) = appel(
        &app,
        Request::post(TICKETS)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "subject": "Sujet", "body": "Description" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    let (statut, corps) = appel(
        &app,
        Request::get(format!("{TICKETS}/7"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(*vus.lock().unwrap(), vec!["jeton", "cle", "jeton", "cle"]);
}

/// Contre-épreuve : un jeton SSO Premium passe seul, la clé ne part pas.
#[tokio::test]
async fn jeton_premium_la_cle_ne_part_pas() {
    let (base, vus) = faux_site(Jeton::Premium).await;
    let app = app_vers(&base, true);
    let (statut, _) = appel(&app, lister()).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(*vus.lock().unwrap(), vec!["jeton"]);
}

/// Contre-épreuve : sans licence sur le serveur, le refus du site reste
/// tel quel (le bandeau Premium est alors juste).
#[tokio::test]
async fn sans_licence_le_refus_premium_reste() {
    let (base, vus) = faux_site(Jeton::SansLicence).await;
    let app = app_vers(&base, false);
    let (statut, corps) = appel(&app, lister()).await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
    assert_eq!(corps["error"], "premium_required", "{corps}");
    assert_eq!(*vus.lock().unwrap(), vec!["jeton"]);
}
