//! rc14 — quand mozaiklabs.fr refuse ou rétrograde une clé, Tune doit DIRE
//! pourquoi le palier est gratuit, au lieu d'un FREE muet.
//!
//! Un essai par raison, chacun contre un faux serveur de licences local
//! (`mozaik_base_url`), jamais mozaiklabs.fr. Les réponses imitent celles de
//! `LicenseController::validate` du site. Chaque essai vérifie aussi la
//! contrepartie : la raison s'AFFICHE, elle n'accorde jamais le Premium.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "TUNE-RAISON-RC14-TEST";
const ACTIVATE: &str = "/api/v1/cloud/license/activate";
const VALIDATE: &str = "/api/v1/cloud/license/validate";
const STATUS: &str = "/api/v1/cloud/license/status";

/// Ce que rend le faux site : un statut HTTP et un corps.
type Reponse = Arc<Mutex<(u16, Value)>>;

async fn faux_site(statut: u16, corps: Value) -> (String, Reponse) {
    let reponse: Reponse = Arc::new(Mutex::new((statut, corps)));
    let partagee = reponse.clone();
    let app = axum::Router::new().route(
        "/api/v1/license/validate",
        axum::routing::post(move || {
            let partagee = partagee.clone();
            async move {
                let (s, c) = partagee.lock().unwrap().clone();
                (StatusCode::from_u16(s).unwrap(), axum::Json(c))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), reponse)
}

/// Une adresse où personne n'écoute : la requête échoue à la connexion.
async fn adresse_morte() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

fn serveur(base_url: &str) -> (axum::Router, tune_core::test_scratch::ScratchDir) {
    let dir = tune_core::test_scratch::scratch_dir("tune-licraison");
    let db = dir.join("library.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", base_url)
        .unwrap();
    (
        tune_server::routes::router(state).layer(axum::extract::connect_info::MockConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 50000)),
        )),
        dir,
    )
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> Value {
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn activer(app: &axum::Router) -> Value {
    let req = Request::post(ACTIVATE)
        .header("content-type", "application/json")
        .body(Body::from(json!({ "license_key": CLE }).to_string()))
        .unwrap();
    appeler(app, req).await
}

async fn statut(app: &axum::Router) -> Value {
    appeler(app, Request::get(STATUS).body(Body::empty()).unwrap()).await
}

async fn valider(app: &axum::Router) -> Value {
    appeler(app, Request::post(VALIDATE).body(Body::empty()).unwrap()).await
}

/// La contrepartie de chaque essai : Free, aucune fonction premium ouverte, et
/// la clé absente du détail de la raison.
fn aucun_premium(statut: &Value) {
    assert_eq!(statut["tier"], "free", "statut : {statut}");
    let premium_ouvert = statut["features"]
        .as_object()
        .unwrap()
        .iter()
        .any(|(nom, f)| f["enabled"] == json!(true) && nom == "unlimited_zones");
    assert!(!premium_ouvert, "le Premium s'est ouvert : {statut}");
    assert!(
        !statut["reason_detail"].to_string().contains(CLE),
        "la clé fuit dans la raison : {statut}"
    );
}

fn iso(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[tokio::test]
async fn session_tenue_ailleurs_dit_l_attente_restante() {
    let depuis = iso(chrono::Utc::now() - chrono::Duration::minutes(30));
    let (base, _) = faux_site(
        200,
        json!({
            "license_valid": false, "license_tier": "free", "session_conflict": true,
            "active_server": "autre-serveur", "active_since": depuis,
        }),
    )
    .await;
    let (app, _d) = serveur(&base);

    let rep = activer(&app).await;
    assert_eq!(rep["status"], "pending", "{rep}");
    assert_eq!(rep["reason"], "session_conflict", "{rep}");

    let s = statut(&app).await;
    assert_eq!(s["reason"], "session_conflict", "{s}");
    let min = s["reason_detail"]["minutes_remaining"].as_i64().unwrap();
    assert!((59..=61).contains(&min), "minutes restantes : {min}");
    assert!(
        !s["reason_detail"].to_string().contains("autre-serveur"),
        "le nom du serveur détenteur n'a pas à être retenu : {s}"
    );
    aucun_premium(&s);
}

#[tokio::test]
async fn premium_echu_avec_modules_dit_la_date() {
    let fin = "2026-10-01T00:00:00+02:00";
    let (base, _) = faux_site(
        200,
        json!({
            "license_valid": true, "license_tier": "free", "premium_active": false,
            "license_expires_at": fin, "modules": ["diretta"],
        }),
    )
    .await;
    let (app, _d) = serveur(&base);

    activer(&app).await;
    let s = statut(&app).await;
    assert_eq!(s["reason"], "key_expired", "{s}");
    assert_eq!(s["reason_detail"]["expires_at"], fin, "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn cle_echue_sans_module_dit_la_date() {
    let fin = "2026-09-01T00:00:00+02:00";
    let (base, _) = faux_site(
        200,
        json!({
            "license_valid": false, "license_tier": "free",
            "message": "License expired", "expired_at": fin,
        }),
    )
    .await;
    let (app, _d) = serveur(&base);

    activer(&app).await;
    let s = statut(&app).await;
    assert_eq!(s["reason"], "key_expired", "{s}");
    assert_eq!(s["reason_detail"]["expires_at"], fin, "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn cle_inconnue_dit_qu_elle_est_invalide() {
    let (base, _) = faux_site(
        200,
        json!({"license_valid": false, "license_tier": "free", "message": "Invalid license key"}),
    )
    .await;
    let (app, _d) = serveur(&base);

    activer(&app).await;
    let s = statut(&app).await;
    assert_eq!(s["reason"], "key_invalid", "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn site_injoignable_le_dit() {
    let (app, _d) = serveur(&adresse_morte().await);

    let rep = activer(&app).await;
    assert_eq!(rep["reason"], "server_unreachable", "{rep}");
    let s = statut(&app).await;
    assert_eq!(s["reason"], "server_unreachable", "{s}");
    assert_eq!(s["reason_detail"]["upstream_status"], Value::Null, "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn site_en_panne_est_injoignable_avec_son_statut() {
    let (base, _) = faux_site(503, json!({"error": "maintenance"})).await;
    let (app, _d) = serveur(&base);

    activer(&app).await;
    let s = statut(&app).await;
    assert_eq!(s["reason"], "server_unreachable", "{s}");
    assert_eq!(s["reason_detail"]["upstream_status"], 503, "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn plafond_de_requetes_le_dit() {
    let (base, _) = faux_site(429, json!({"message": "Too Many Attempts."})).await;
    let (app, _d) = serveur(&base);

    activer(&app).await;
    let v = valider(&app).await;
    // `reason` de #3906 inchangé ; la raison du palier est dans le détail.
    assert_eq!(v["reason"], "upstream_error", "{v}");
    assert_eq!(v["reason_detail"]["reason"], "rate_limited", "{v}");
    let s = statut(&app).await;
    assert_eq!(s["reason"], "rate_limited", "{s}");
    aucun_premium(&s);
}

#[tokio::test]
async fn premium_hors_ligne_dit_la_grace_et_sa_fin() {
    let (base, reponse) = faux_site(
        200,
        json!({"license_valid": true, "license_tier": "premium", "license_expires_at": null}),
    )
    .await;
    let (app, _d) = serveur(&base);

    let rep = activer(&app).await;
    assert_eq!(rep["tier"], "premium", "{rep}");
    assert_eq!(rep["reason"], Value::Null, "rien à dire d'un Premium confirmé : {rep}");

    // Le site tombe : le Premium déjà validé reste, et la grâce se dit.
    *reponse.lock().unwrap() = (503, json!({}));
    let v = valider(&app).await;
    assert_eq!(v["tier"], "premium", "{v}");
    assert_eq!(v["reason_detail"]["reason"], "grace_period", "{v}");
    let s = statut(&app).await;
    assert_eq!(s["reason"], "grace_period", "{s}");
    assert!(
        s["reason_detail"]["grace_until"].as_str().is_some(),
        "la fin de la grâce doit être donnée : {s}"
    );
}

#[tokio::test]
async fn une_nouvelle_cle_oublie_la_raison_de_l_ancienne() {
    let (base, reponse) = faux_site(
        200,
        json!({"license_valid": false, "license_tier": "free", "message": "Invalid license key"}),
    )
    .await;
    let (app, _d) = serveur(&base);
    activer(&app).await;
    assert_eq!(statut(&app).await["reason"], "key_invalid");

    *reponse.lock().unwrap() = (
        200,
        json!({"license_valid": true, "license_tier": "premium", "license_expires_at": null}),
    );
    activer(&app).await;
    let s = statut(&app).await;
    assert_eq!(s["tier"], "premium", "{s}");
    assert_eq!(s["reason"], Value::Null, "{s}");
}
