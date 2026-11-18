//! TESS T17 — préréglages de chaîne et mode d'application, par les VRAIES
//! routes (`/api/v1/tess/presets…`, `/api/v1/zones/{id}/tess/presets…`).
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::tess::migration::{ecrire_chaine, lire_zone};
use tune_core::tess::{ChaineTess, MaillonGreffon};

async fn etat(premium: bool) -> (tune_server::state::AppState, axum::Router, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    for id in ["equalizer", "crossfeed"] {
        settings
            .set(&format!("plugin_{id}_installed"), "true")
            .unwrap();
        settings
            .set(&format!("plugin_{id}_enabled"), "true")
            .unwrap();
    }
    state.license.set_account_premium(premium, None).await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app, zone)
}

async fn appel(
    app: &axum::Router,
    methode: Method,
    uri: &str,
    corps: Option<String>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(corps.map_or(Body::empty(), Body::from))
        .unwrap();
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

fn crossfeed() -> ChaineTess {
    ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "crossfeed".into(),
            enabled: true,
            settings: json!({ "amount": 0.3 }),
        }],
        ..ChaineTess::default()
    }
}

#[tokio::test]
async fn sauver_charger_actif_et_mode() {
    let (state, app, z) = etat(true).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Liste par défaut : Pure, fourni par le serveur.
    let (s, l) = appel(&app, Method::GET, "/api/v1/tess/presets", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l["apply_mode"], "immediately");
    assert_eq!(l["presets"][0]["id"], "pure");
    assert_eq!(l["presets"][0]["name"], "Pure — no processing");
    assert_eq!(l["presets"][0]["builtin"], true);

    // « Save » : la chaîne courante de la zone, nommée.
    ecrire_chaine(&settings, z, &crossfeed()).unwrap();
    let (s, p) = appel(
        &app,
        Method::POST,
        "/api/v1/tess/presets",
        Some(json!({ "name": "Headphones", "zone_id": z }).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["id"], "headphones");
    let uri_zone = format!("/api/v1/zones/{z}/tess/presets");
    let (_, v) = appel(&app, Method::GET, &uri_zone, None).await;
    assert_eq!(v["active_preset"], "headphones");

    // Mode « au prochain démarrage » (aucune interruption).
    let (s, m) = appel(
        &app,
        Method::PUT,
        "/api/v1/tess/presets/apply_mode",
        Some(json!({ "apply_mode": "at_next_playback" }).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(m["apply_mode"], "at_next_playback");

    // « Load » Pure : la chaîne de la zone est remplacée, rien ne joue.
    let (s, r) = appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{z}/tess/presets/pure/load"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["active_preset"], "pure");
    assert_eq!(r["chain"]["bypass_all"], true);
    assert_eq!(r["application"], "at_next_playback");
    assert!(lire_zone(&settings, z, true).unwrap().chaine.bypass_all);
    let (_, v) = appel(&app, Method::GET, &uri_zone, None).await;
    assert_eq!(v["active_preset"], "pure");

    // Mode immédiat, rien ne joue : `not_playing`.
    appel(
        &app,
        Method::PUT,
        "/api/v1/tess/presets/apply_mode",
        Some(json!({ "apply_mode": "immediately" }).to_string()),
    )
    .await;
    let (_, r) = appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{z}/tess/presets/headphones/load"),
        None,
    )
    .await;
    assert_eq!(r["application"], "not_playing");
    assert_eq!(lire_zone(&settings, z, true).unwrap().chaine, crossfeed());

    // Suppression.
    let (s, _) = appel(
        &app,
        Method::DELETE,
        "/api/v1/tess/presets/headphones",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = appel(&app, Method::GET, &uri_zone, None).await;
    assert_eq!(v["active_preset"], Value::Null);
}

#[tokio::test]
async fn refus_coherents_400_402_404() {
    let (state, app, z) = etat(false).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Corps strict.
    for corps in [
        "{oups".to_owned(),
        json!({ "name": "x", "zone_id": z, "color": "red" }).to_string(),
    ] {
        let (s, r) = appel(&app, Method::POST, "/api/v1/tess/presets", Some(corps)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(r["code"], "invalid_body");
    }
    let (s, r) = appel(
        &app,
        Method::PUT,
        "/api/v1/tess/presets/apply_mode",
        Some(json!({ "apply_mode": "crossfade" }).to_string()),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    // Zone et préréglage inconnus.
    let (s, r) = appel(
        &app,
        Method::POST,
        "/api/v1/tess/presets",
        Some(json!({ "name": "x", "zone_id": 999 }).to_string()),
    )
    .await;
    assert_eq!(
        (s, r["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown_zone"))
    );
    let (s, r) = appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{z}/tess/presets/nope/load"),
        None,
    )
    .await;
    assert_eq!(
        (s, r["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown_preset"))
    );
    // Un préréglage Premium chargé sans Premium : 402, la zone ne change pas.
    let premium = ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "convolution".into(),
            enabled: true,
            settings: json!({ "ir_path": "/tmp/room.wav" }),
        }],
        ..ChaineTess::default()
    };
    tune_core::tess::prereglages_stockage::enregistrer_la_chaine(&settings, "Room", &premium)
        .unwrap();
    let avant = lire_zone(&settings, z, false).unwrap().chaine;
    let (s, r) = appel(
        &app,
        Method::POST,
        &format!("/api/v1/zones/{z}/tess/presets/room/load"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{r}");
    assert_eq!(r["code"], "premium_required");
    assert_eq!(r["plugin_id"], "convolution");
    assert_eq!(lire_zone(&settings, z, false).unwrap().chaine, avant);
}
