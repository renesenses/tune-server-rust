//! #5069 — la carte « Compensation de niveau » annonçait « +8.4 dB rendus par
//! le volume » alors que le volume de la zone était déjà à 100 % : la demande
//! est multipliée au volume puis rabotée à l'unité, donc à plein volume rien
//! n'est rendu. Le testeur perdait 8,4 dB sans que l'écran le dise.
//!
//! La réserve elle-même (`automatic_headroom_db_at`, norme L1) est VOULUE et
//! n'est pas en cause. Ce fichier cloue ce que `GET /zones/{id}/dsp` publie
//! désormais : `rendered_db` (ce que le volume COURANT rend réellement) et
//! `unrendered_db` (ce que le rabot mange), à trois volumes.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

/// Serveur en mémoire, égaliseur installé, une zone locale avec un
/// égaliseur qui pousse (+6 dB sur trois bandes) : la réserve anti-écrêtage
/// retire plusieurs dB au niveau moyen, que la compensation demande au volume.
async fn app() -> (axum::Router, i64, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    let grille = [31, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    let gains = [6.0, 6.0, 6.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let profil = json!({
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
    });
    s.set(&format!("zone_{zone}_eq_profile"), &profil.to_string())
        .unwrap();
    (tune_server::routes::router(state.clone()), zone, state)
}

async fn lire(app: &axum::Router, zone: i64) -> Value {
    let res = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/zones/{zone}/dsp"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap();
    corps["level_compensation"].clone()
}

fn nombre(lc: &Value, cle: &str) -> f64 {
    lc[cle]
        .as_f64()
        .unwrap_or_else(|| panic!("`{cle}` absent de level_compensation : {lc}"))
}

#[tokio::test]
async fn au_volume_maximal_rien_n_est_rendu() {
    let (app, zone, state) = app().await;
    state.playback.set_volume(zone, 1.0).await;
    let lc = lire(&app, zone).await;
    eprintln!("100 % : {lc}");
    let comp = nombre(&lc, "compensation_db");
    assert!(
        comp > 3.0,
        "le banc doit demander une compensation nette : {lc}"
    );
    assert_eq!(
        nombre(&lc, "rendered_db"),
        0.0,
        "volume à 100 % : la route annonce des dB rendus alors que le rabot à l'unité les mange tous : {lc}"
    );
    assert!(
        (nombre(&lc, "unrendered_db") - comp).abs() < 0.011,
        "volume à 100 % : toute la compensation est perdue : {lc}"
    );
}

#[tokio::test]
async fn a_mi_course_seule_la_marge_du_volume_est_rendue() {
    let (app, zone, state) = app().await;
    // 0,5 linéaire = −6,02 dB : la marge jusqu'à la pleine échelle.
    state.playback.set_volume(zone, 0.5).await;
    let lc = lire(&app, zone).await;
    eprintln!("50 % : {lc}");
    let comp = nombre(&lc, "compensation_db");
    let rendu = nombre(&lc, "rendered_db");
    let perdu = nombre(&lc, "unrendered_db");
    let marge = -20.0 * 0.5_f64.log10();
    assert!(
        (rendu - comp.min(marge)).abs() < 0.011,
        "50 % : rendu {rendu} au lieu de min({comp}, {marge:.2}) : {lc}"
    );
    assert!(
        (rendu + perdu - comp).abs() < 0.011,
        "rendu + perdu = demandé : {lc}"
    );
}

#[tokio::test]
async fn a_bas_volume_tout_est_rendu() {
    let (app, zone, state) = app().await;
    // 0,1 linéaire = −20 dB de marge : bien plus que la réserve de l'égaliseur.
    state.playback.set_volume(zone, 0.1).await;
    let lc = lire(&app, zone).await;
    let comp = nombre(&lc, "compensation_db");
    assert!((nombre(&lc, "rendered_db") - comp).abs() < 0.011, "{lc}");
    assert_eq!(nombre(&lc, "unrendered_db"), 0.0, "{lc}");
}

#[tokio::test]
async fn eteinte_rien_n_est_demande_donc_rien_n_est_perdu() {
    let (app, zone, state) = app().await;
    state.playback.set_volume(zone, 1.0).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &tune_core::orchestrator::PlaybackOrchestrator::cle_compensation_de_niveau(zone),
            "false",
        )
        .unwrap();
    let lc = lire(&app, zone).await;
    assert_eq!(lc["enabled"], false, "{lc}");
    assert_eq!(nombre(&lc, "rendered_db"), 0.0, "{lc}");
    assert_eq!(nombre(&lc, "unrendered_db"), 0.0, "{lc}");
}
