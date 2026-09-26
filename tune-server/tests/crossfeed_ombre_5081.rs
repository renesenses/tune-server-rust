//! #5081 — l'ombre de la tête du crossfeed par les routes : `GET`/`PUT
//! /zones/{id}/dsp` et `/crossfeed/presets`, par le routeur réel.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn etat() -> (tune_server::state::AppState, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    settings.set("plugin_crossfeed_installed", "true").unwrap();
    settings.set("plugin_crossfeed_enabled", "true").unwrap();
    state.license.set_account_premium(true, None).await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    (state, zone)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn lire(app: &axum::Router, zone: i64) -> Value {
    let (status, corps) = appel(
        app,
        Request::get(format!("/api/v1/zones/{zone}/dsp"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    corps
}

async fn ecrire(app: &axum::Router, zone: i64, crossfeed: Value) -> Value {
    let (status, corps) = appel(
        app,
        Request::put(format!("/api/v1/zones/{zone}/dsp"))
            .header("Content-Type", "application/json")
            .body(Body::from(json!({ "crossfeed": crossfeed }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    corps
}

async fn poster(app: &axum::Router, corps: Value) -> Value {
    let (status, corps) = appel(
        app,
        Request::post("/api/v1/crossfeed/presets")
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "{status} {corps}");
    corps
}

/// Jamais réglé : éteint, 700 Hz, 6 dB/oct ; et les bornes sont publiées —
/// c'est à elles qu'un client reconnaît un serveur qui connaît le filtre.
#[tokio::test]
async fn get_dsp_publie_le_filtre_eteint_et_ses_bornes_5081() {
    let (state, zone) = etat().await;
    let app = tune_server::routes::router(state);
    let corps = lire(&app, zone).await;
    assert_eq!(corps["crossfeed"]["head_shadow_enabled"], false);
    assert_eq!(corps["crossfeed"]["cutoff_hz"], 700.0);
    assert_eq!(corps["crossfeed"]["slope_db_per_octave"], 6.0);
    let bornes = &corps["crossfeed_limits"];
    assert_eq!(bornes["cutoff_hz_min"], 200.0);
    assert_eq!(bornes["cutoff_hz_max"], 20000.0);
    assert_eq!(bornes["slope_db_per_octave_min"], 3.0);
    assert_eq!(bornes["slope_db_per_octave_max"], 6.0);
}

/// `PUT` enregistre le filtre, borné ; un client d'avant #5081 (sans les
/// champs) ne l'éteint pas en changeant l'intensité.
#[tokio::test]
async fn put_dsp_enregistre_borne_et_preserve_le_filtre_5081() {
    let (state, zone) = etat().await;
    let app = tune_server::routes::router(state);
    let reponse = ecrire(
        &app,
        zone,
        json!({"enabled": true, "amount": 0.3, "delay_ms": 0.3,
               "head_shadow_enabled": true, "cutoff_hz": 1200.0, "slope_db_per_octave": 3.0}),
    )
    .await;
    assert_eq!(reponse["crossfeed"]["head_shadow_enabled"], true);
    assert_eq!(reponse["crossfeed"]["cutoff_hz"], 1200.0);
    assert_eq!(reponse["crossfeed"]["slope_db_per_octave"], 3.0);

    // Client d'avant : seulement l'intensité.
    ecrire(
        &app,
        zone,
        json!({"enabled": true, "amount": 0.4, "delay_ms": 0.3}),
    )
    .await;
    let relu = lire(&app, zone).await;
    assert_eq!(relu["crossfeed"]["amount"], 0.4);
    assert_eq!(
        relu["crossfeed"]["head_shadow_enabled"], true,
        "un client d'avant #5081 a éteint le filtre"
    );
    assert_eq!(relu["crossfeed"]["cutoff_hz"], 1200.0);

    // Hors bornes : ramené dans l'échelle.
    let reponse = ecrire(
        &app,
        zone,
        json!({"enabled": true, "head_shadow_enabled": true,
               "cutoff_hz": 50.0, "slope_db_per_octave": 12.0}),
    )
    .await;
    assert_eq!(reponse["crossfeed"]["cutoff_hz"], 200.0);
    assert_eq!(reponse["crossfeed"]["slope_db_per_octave"], 6.0);
}

/// Un préréglage porte le filtre ; un préréglage d'avant #5081, sans les
/// champs, se relit filtre ÉTEINT.
#[tokio::test]
async fn les_prereglages_portent_le_filtre_et_l_ancien_se_relit_eteint_5081() {
    let (state, _) = etat().await;
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "crossfeed_presets",
            &json!([{"id": "ancien", "name": "Ancien", "amount": 0.3, "delay_ms": 0.3,
                      "created_at": 0}])
            .to_string(),
        )
        .unwrap();
    let app = tune_server::routes::router(state);
    let cree = poster(
        &app,
        json!({"name": "Meier étendu", "amount": 0.3, "delay_ms": 0.3,
               "head_shadow_enabled": true, "cutoff_hz": 1200.0, "slope_db_per_octave": 3.0}),
    )
    .await;
    assert_eq!(cree["head_shadow_enabled"], true);
    assert_eq!(cree["cutoff_hz"], 1200.0);
    let simple = poster(
        &app,
        json!({"name": "Simple", "amount": 0.3, "delay_ms": 0.3}),
    )
    .await;
    assert_eq!(simple["head_shadow_enabled"], false);

    let (_, liste) = appel(
        &app,
        Request::get("/api/v1/crossfeed/presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let ancien = liste["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "ancien")
        .unwrap()
        .clone();
    assert_eq!(ancien["head_shadow_enabled"], false);
    assert_eq!(ancien["cutoff_hz"], 700.0);
    assert_eq!(ancien["slope_db_per_octave"], 6.0);
}
