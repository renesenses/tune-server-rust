//! #4685 — l'interrupteur de compensation de niveau, par le routeur réel.
//!
//! `GET /zones/{id}/dsp` publie `level_compensation` : l'interrupteur (actif
//! par défaut), ce que l'égaliseur et le crossfeed de la zone font au niveau
//! MOYEN, et ce que la sortie locale rend par le volume. `PUT` l'écrit et
//! rend le nouvel état. Les nombres viennent des MÊMES chargeurs que la
//! lecture (`load_eq_processor`, `load_crossfeed_processor`) : une zone sans
//! greffon installé ou en PURE ne compense rien.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

/// Le préréglage « Rock » (grille ISO 10 bandes, Q = 1), tel que l'écrit le
/// client.
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

/// Serveur en mémoire, Premium, égaliseur et crossfeed installés, une zone
/// locale réglée sur « Rock » + crossfeed 30 % sans retard.
async fn app() -> (axum::Router, i64, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    for id in ["equalizer", "crossfeed"] {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    }
    s.set(
        &format!("zone_{zone}_eq_profile"),
        &profil_rock().to_string(),
    )
    .unwrap();
    s.set(
        &format!("zone_{zone}_crossfeed"),
        r#"{"enabled":true,"amount":0.30,"delay_ms":0.0}"#,
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

async fn lire(app: &axum::Router, zone: i64) -> Value {
    let (statut, corps) = reponse(
        app,
        Request::get(format!("/api/v1/zones/{zone}/dsp"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps["level_compensation"].clone()
}

#[tokio::test]
async fn la_route_publie_la_compensation_active_par_defaut_et_chiffree() {
    let (app, zone, _state) = app().await;
    let lc = lire(&app, zone).await;
    eprintln!("level_compensation = {lc}");
    assert_eq!(lc["enabled"], true, "active par défaut : {lc}");
    let eq = lc["eq_db"].as_f64().unwrap();
    let cf = lc["crossfeed_db"].as_f64().unwrap();
    let comp = lc["compensation_db"].as_f64().unwrap();
    // Témoins chiffrés des greffons : Rock ≈ −9,36 dB au niveau moyen,
    // crossfeed 30 % sans retard ≈ −1,02 dB.
    assert!((eq - (-9.36)).abs() < 0.05, "égaliseur {eq}");
    assert!((cf - (-1.02)).abs() < 0.05, "crossfeed {cf}");
    assert!((comp - -(eq + cf)).abs() < 0.011, "compensation {comp}");
    assert_eq!(lc["local_output_only"], true);
}

#[tokio::test]
async fn l_interrupteur_s_ecrit_et_se_relit() {
    let (app, zone, _state) = app().await;
    let (statut, corps) = reponse(
        &app,
        Request::put(format!("/api/v1/zones/{zone}/dsp"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"level_compensation": {"enabled": false}}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["level_compensation"]["enabled"], false, "{corps}");
    assert_eq!(corps["level_compensation"]["compensation_db"], 0.0);
    // Rien ne joue : rien n'a été poussé à une sortie vivante.
    assert_eq!(corps["level_compensation_applied_live"], false);
    // Éteinte, elle publie toujours ce que le DSP retire : l'écran peut le
    // dire.
    let lc = lire(&app, zone).await;
    assert_eq!(lc["enabled"], false);
    assert!(lc["eq_db"].as_f64().unwrap() < -9.0, "{lc}");
}

#[tokio::test]
async fn en_pure_rien_n_est_a_compenser() {
    let (app, zone, state) = app().await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let lc = lire(&app, zone).await;
    assert_eq!(lc["eq_db"], 0.0, "{lc}");
    assert_eq!(lc["crossfeed_db"], 0.0, "{lc}");
    assert_eq!(lc["compensation_db"], 0.0, "{lc}");
}
