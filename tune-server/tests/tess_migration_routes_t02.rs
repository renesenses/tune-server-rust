//! TESS T02 — l'écriture croisée par les VRAIES anciennes routes :
//! `PUT /zones/{id}/dsp` (égaliseur, crossfeed), `PATCH /zones/{id}` (repli
//! mono), `PUT /channel-remap/zones/{id}`, `PUT /upsampler/zones/{id}`.
//!
//! - aller-retour : ancienne route → TESS (lecture puis réécriture de la
//!   chaîne) → ancienne route : les `GET` rendent la même chose ;
//! - TESS → ancienne route : une chaîne écrite par TESS se lit par les `GET`
//!   d'aujourd'hui (clients mobiles) ;
//! - ancienne route → TESS : un `PUT` d'aujourd'hui se voit dans la chaîne.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::tess::migration::{ID_CROSSFEED, ID_MONO, ecrire_chaine, lire_zone};

async fn etat() -> (tune_server::state::AppState, axum::Router, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    for id in ["equalizer", "crossfeed", "channel-remap", "upsampler"] {
        settings
            .set(&format!("plugin_{id}_installed"), "true")
            .unwrap();
        settings
            .set(&format!("plugin_{id}_enabled"), "true")
            .unwrap();
    }
    state.license.set_account_premium(true, None).await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app, zone)
}

async fn appel(app: &axum::Router, methode: Method, uri: String, corps: Option<Value>) -> Value {
    let req = Request::builder()
        .method(methode.clone())
        .uri(&uri)
        .header("Content-Type", "application/json")
        .body(corps.map_or(Body::empty(), |c| Body::from(c.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
    assert_eq!(status, StatusCode::OK, "{methode} {uri} : {v}");
    v
}

/// Ce que les anciennes routes rendent des réglages de la zone.
async fn photo(app: &axum::Router, z: i64) -> Value {
    let dsp = appel(app, Method::GET, format!("/api/v1/zones/{z}/dsp"), None).await;
    let zone = appel(app, Method::GET, format!("/api/v1/zones/{z}"), None).await;
    let remap = appel(
        app,
        Method::GET,
        format!("/api/v1/channel-remap/zones/{z}"),
        None,
    )
    .await;
    let up = appel(
        app,
        Method::GET,
        format!("/api/v1/upsampler/zones/{z}"),
        None,
    )
    .await;
    json!({
        "eq_profile": dsp["eq_profile"],
        "crossfeed": dsp["crossfeed"],
        "mono_downmix": zone["mono_downmix"],
        "channel_remap": remap,
        "upsampler": up,
    })
}

async fn poser_par_les_anciennes_routes(app: &axum::Router, z: i64) {
    appel(
        app,
        Method::PUT,
        format!("/api/v1/zones/{z}/dsp"),
        Some(json!({
            "eq_profile": {
                "enabled": true, "listening": "speakers", "room_size": "medium",
                "speaker_placement": "free_standing", "bass_gain_db": 0.0,
                "mid_gain_db": 0.0, "treble_gain_db": 0.0,
                "bands": [
                    {"freq": 80.0, "gain": 4.0, "q": 0.7, "type": "low_shelf"},
                    {"freq": 1200.0, "gain": -3.5, "q": 1.4, "type": "peak"}
                ]
            },
            "crossfeed": {"enabled": true, "amount": 0.35, "delay_ms": 0.3}
        })),
    )
    .await;
    appel(
        app,
        Method::PATCH,
        format!("/api/v1/zones/{z}"),
        Some(json!({"mono_downmix": true})),
    )
    .await;
    appel(
        app,
        Method::PUT,
        format!("/api/v1/channel-remap/zones/{z}"),
        Some(json!({"enabled": true, "inputs": 2, "outputs": 2,
                    "gains_db": [[null, 0.0], [0.0, null]], "normalize": true})),
    )
    .await;
    appel(
        app,
        Method::PUT,
        format!("/api/v1/upsampler/zones/{z}"),
        Some(json!({"enabled": false, "filter": "precision", "max_ratio": 4})),
    )
    .await;
}

#[tokio::test]
async fn aller_retour_ancienne_route_tess_ancienne_route_identique_t02() {
    let (state, app, z) = etat().await;
    poser_par_les_anciennes_routes(&app, z).await;
    let avant = photo(&app, z).await;
    assert_eq!(avant["mono_downmix"], json!(true), "{avant}");
    assert_eq!(avant["crossfeed"]["enabled"], json!(true), "{avant}");

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let chaine = lire_zone(&settings, z, true).unwrap().chaine;
    let bilan = ecrire_chaine(&settings, z, &chaine).unwrap();
    assert!(
        bilan.cles_modifiees.is_empty(),
        "{:?}",
        bilan.cles_modifiees
    );

    assert_eq!(photo(&app, z).await, avant);
}

#[tokio::test]
async fn tess_puis_ancienne_route_dans_les_deux_sens_t02() {
    let (state, app, z) = etat().await;
    poser_par_les_anciennes_routes(&app, z).await;
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // TESS décoche le crossfeed et retire le repli mono : les anciennes
    // routes (clients mobiles) le voient.
    let mut chaine = lire_zone(&settings, z, true).unwrap().chaine;
    chaine
        .plugins
        .iter_mut()
        .find(|m| m.id == ID_CROSSFEED)
        .unwrap()
        .enabled = false;
    chaine.plugins.retain(|m| m.id != ID_MONO);
    ecrire_chaine(&settings, z, &chaine).unwrap();
    let vu = photo(&app, z).await;
    assert_eq!(vu["crossfeed"]["enabled"], json!(false), "{vu}");
    assert_eq!(
        vu["crossfeed"]["amount"]
            .as_f64()
            .map(|a| (a * 100.0).round()),
        Some(35.0)
    );
    assert_eq!(vu["mono_downmix"], json!(false), "{vu}");

    // Une ancienne route rallume le crossfeed à un autre niveau (borné à 0,5 par la route) : TESS lit ce qu'elle a enregistré.
    appel(
        &app,
        Method::PUT,
        format!("/api/v1/zones/{z}/dsp"),
        Some(json!({"crossfeed": {"enabled": true, "amount": 0.45, "delay_ms": 0.3}})),
    )
    .await;
    let relue = lire_zone(&settings, z, true).unwrap().chaine;
    let cf = relue.plugins.iter().find(|m| m.id == ID_CROSSFEED).unwrap();
    assert!(cf.enabled);
    assert_eq!(
        cf.settings["amount"].as_f64().map(|a| (a * 100.0).round()),
        Some(45.0)
    );
    assert!(relue.plugins.iter().all(|m| m.id != ID_MONO));
}
