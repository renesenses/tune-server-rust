//! #5171 — le réglage « Réserve » de l'égaliseur, par le routeur réel.
//!
//! `GET /zones/{id}/eq` publie `headroom_mode` (sa présence dit au client que
//! le serveur connaît le réglage), `POST` l'écrit, `PUT /zones/{id}/dsp` avec
//! un `eq_profile` qui ne le porte pas le GARDE, et la compensation de niveau
//! (`level_compensation`) suit la réserve réellement appliquée.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

/// Le préréglage « Rock » (grille ISO 10 bandes, Q = 1).
fn profil_rock() -> Value {
    let grille = [31, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    let gains = [5.0, 3.0, 0.0, -2.0, -1.0, 2.0, 4.0, 5.0, 5.0, 4.0];
    json!({
        "enabled": true,
        "listening": "speakers",
        "room_size": "medium",
        "speaker_placement": "free_standing",
        "bass_gain_db": 0.0,
        "mid_gain_db": 0.0,
        "treble_gain_db": 0.0,
        "bands": grille.iter().zip(gains).map(|(f, g)| json!({
            "freq": f, "gain": g, "q": 1.0, "type": "peak"
        })).collect::<Vec<_>>(),
    })
}

async fn app() -> (axum::Router, i64, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    s.set(
        &format!("zone_{zone}_eq_profile"),
        &profil_rock().to_string(),
    )
    .unwrap();
    (tune_server::routes::router(state.clone()), zone, state)
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
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

async fn get(app: &axum::Router, uri: String) -> Value {
    let (statut, corps) = reponse(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    uri: String,
    corps: Value,
) -> (StatusCode, Value) {
    reponse(
        app,
        Request::builder()
            .method(methode)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

fn mode_enregistre(state: &tune_server::state::AppState, zone: i64) -> Option<String> {
    let brut = SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("zone_{zone}_eq_profile"))
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&brut).unwrap();
    v.get("headroom_mode")
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

#[tokio::test]
async fn la_reserve_se_lit_s_ecrit_et_suit_la_compensation() {
    let (app, zone, state) = app().await;

    // Par défaut : sûre, publiée, et absente du profil enregistré.
    let eq = get(&app, format!("/api/v1/zones/{zone}/eq")).await;
    assert_eq!(eq["headroom_mode"], "safe", "{eq}");
    let sure = get(&app, format!("/api/v1/zones/{zone}/dsp")).await["level_compensation"].clone();

    // Réaliste.
    let (statut, corps) = envoyer(
        &app,
        "POST",
        format!("/api/v1/zones/{zone}/eq"),
        json!({"headroom_mode": "realistic"}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["headroom_mode"], "realistic", "{corps}");
    assert_eq!(mode_enregistre(&state, zone).as_deref(), Some("realistic"));
    let eq = get(&app, format!("/api/v1/zones/{zone}/eq")).await;
    assert_eq!(eq["headroom_mode"], "realistic", "{eq}");
    assert_eq!(
        eq["bands"].as_array().unwrap().len(),
        10,
        "les bandes restent : {eq}"
    );

    // La compensation suit la réserve RÉELLEMENT appliquée : l'égaliseur
    // retire moins au niveau moyen, il y a donc moins à rendre.
    let realiste =
        get(&app, format!("/api/v1/zones/{zone}/dsp")).await["level_compensation"].clone();
    let (eq_sure, eq_realiste) = (
        sure["eq_db"].as_f64().unwrap(),
        realiste["eq_db"].as_f64().unwrap(),
    );
    assert!(
        eq_realiste > eq_sure + 3.0,
        "la compensation n'a pas suivi la réserve : sûre {sure}, réaliste {realiste}"
    );
    assert!(
        (realiste["compensation_db"].as_f64().unwrap() + eq_realiste).abs() < 0.011,
        "{realiste}"
    );

    // Un corps `eq_profile` qui ne porte pas le réglage (écran « Profil
    // acoustique », client d'avant #5171) ne le ramène pas à « Sûre ».
    let (statut, corps) = envoyer(
        &app,
        "PUT",
        format!("/api/v1/zones/{zone}/dsp"),
        json!({"eq_profile": profil_rock()}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        mode_enregistre(&state, zone).as_deref(),
        Some("realistic"),
        "PUT /dsp sans `headroom_mode` a remis la réserve à « Sûre »"
    );

    // …mais un corps qui le porte le change.
    let mut sur = profil_rock();
    sur["headroom_mode"] = "safe".into();
    let (statut, _) = envoyer(
        &app,
        "PUT",
        format!("/api/v1/zones/{zone}/dsp"),
        json!({"eq_profile": sur}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        mode_enregistre(&state, zone),
        None,
        "« Sûre » s'écrit comme avant"
    );
}

#[tokio::test]
async fn une_reserve_inconnue_est_refusee() {
    let (app, zone, state) = app().await;
    let (statut, corps) = envoyer(
        &app,
        "POST",
        format!("/api/v1/zones/{zone}/eq"),
        json!({"headroom_mode": "turbo"}),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(mode_enregistre(&state, zone), None);
}
