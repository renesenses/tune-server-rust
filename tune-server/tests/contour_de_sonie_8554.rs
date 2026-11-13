//! #8554 — `GET` / `POST /zones/{id}/eq/loudness`, par le routeur réel.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

async fn app(premium: bool) -> (axum::Router, i64, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    if premium {
        state.license.set_account_premium(true, None).await;
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    (tune_server::routes::router(state.clone()), zone, state)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.unwrap();
    let statut = res.status();
    let octets = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn get(zone: i64) -> Request<Body> {
    Request::get(format!("/api/v1/zones/{zone}/eq/loudness"))
        .body(Body::empty())
        .unwrap()
}

fn post(zone: i64, corps: Value) -> Request<Body> {
    Request::post(format!("/api/v1/zones/{zone}/eq/loudness"))
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap()
}

#[tokio::test]
async fn par_defaut_le_contour_est_eteint() {
    let (app, zone, _) = app(false).await;
    let (statut, corps) = appel(&app, get(zone)).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["status"], "disabled");
    assert_eq!(corps["settings"]["enabled"], false);
    assert_eq!(corps["settings"]["curve"], "iso226_2003");
    assert_eq!(corps["settings"]["intensity_pct"], 100.0);
    assert_eq!(corps["settings"]["reference_phon"], 80.0);
    assert_eq!(corps["settings"]["level_source"], "volume");
}

#[tokio::test]
async fn ecrire_exige_le_droit_de_l_egaliseur() {
    // Greffon égaliseur désinstallé : la même porte que `POST /zones/{id}/eq`.
    let (app, zone, state) = app(true).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_equalizer_installed", "false")
        .unwrap();
    let (statut, _) = appel(&app, post(zone, json!({"enabled": true}))).await;
    assert!(statut.is_client_error(), "{statut}");
    let eq = Request::post(format!("/api/v1/zones/{zone}/eq"))
        .header("content-type", "application/json")
        .body(Body::from(json!({"enabled": true}).to_string()))
        .unwrap();
    let (statut_eq, _) = appel(&app, eq).await;
    assert_eq!(statut, statut_eq, "même refus que l'égaliseur");
    let (_, corps) = appel(&app, get(zone)).await;
    assert_eq!(corps["settings"]["enabled"], false, "rien n'a été écrit");
}

#[tokio::test]
async fn allumer_rend_le_contour_au_volume_de_la_zone() {
    let (app, zone, state) = app(true).await;
    ZoneRepo::with_backend(state.backend.clone())
        .update_volume(zone, 0.1)
        .unwrap();
    let (statut, corps) = appel(
        &app,
        post(
            zone,
            json!({"enabled": true, "bass_only": true, "intensity_pct": 50}),
        ),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "active");
    assert_eq!(corps["applied"], "next_play");
    assert_eq!(corps["settings"]["bass_only"], true);
    assert_eq!(corps["attenuation_db"], 20.0);
    let points = corps["contour_db"].as_array().unwrap();
    let a = |hz: f64| {
        points.iter().find(|p| p["hz"] == hz).unwrap()["db"]
            .as_f64()
            .unwrap()
    };
    assert!(
        (a(63.0) - 3.8).abs() < 0.11,
        "63 Hz : {} (moitié de +7,6)",
        a(63.0)
    );
    assert_eq!(a(10000.0), 0.0, "grave seulement");
    assert_eq!(a(1000.0), 0.0);
}

#[tokio::test]
async fn une_valeur_hors_bornes_est_refusee() {
    let (app, zone, _) = app(true).await;
    for corps in [
        json!({"intensity_pct": 150}),
        json!({"reference_volume_db": 6}),
        json!({"curve": "fletcher_munson"}),
    ] {
        let (statut, reponse) = appel(&app, post(zone, corps.clone())).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps} → {reponse}");
    }
}

#[tokio::test]
async fn sous_pure_le_contour_est_dit_contourne() {
    let (app, zone, state) = app(true).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let (_, eteint) = appel(&app, get(zone)).await;
    assert_eq!(eteint["status"], "disabled");
    assert_eq!(eteint["pure"], true, "PURE se dit même contour éteint");
    let (statut, corps) = appel(&app, post(zone, json!({"enabled": true}))).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "bypassed_pure");
}
