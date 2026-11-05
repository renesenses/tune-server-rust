//! #7762 — sur une sortie locale (curseur `db60`), la position 50 du curseur
//! vaut le gain 0,0316 (−30 dB). Le chemin du signal affichait ce GAIN comme
//! s'il s'agissait du curseur : « Volume logiciel 3 % » face à un curseur à 50.
//!
//! Témoins sans son audible : la sortie est une `MockOutput`, aucune carte son
//! n'est ouverte.

use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use tune_core::audio::volume_scale::{EchelleDeVolume, gain_perceptif};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::mock::MockOutput;
use tune_core::outputs::traits::{OutputSampleTransport, OutputSignalPathStatus};
use tune_core::playback::NowPlaying;

const LOCALE: &str = "local:dac-7762";

fn description_volume(sp: &Value) -> Option<String> {
    sp["steps"]
        .as_array()?
        .iter()
        .find(|step| step["name"] == "Volume")?["description"]
        .as_str()
        .map(str::to_owned)
}

fn wav24_en_lecture() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("wav".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-7762".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// La sonde de la sortie locale Windows : volume logiciel appliqué, seule
/// cause nommée.
fn sonde_volume_applique() -> OutputSignalPathStatus {
    OutputSignalPathStatus {
        bit_perfect: false,
        sample_transport: OutputSampleTransport::NativeInteger,
        dsp: OutputDspState::Inactive,
        volume: OutputVolumeState::Applied,
        reasons: vec![OutputSignalReason::SoftwareVolume],
    }
}

/// Une zone en base, de type `output_type`, au gain `gain` (0..1).
fn zone_au_gain(output_type: &str, gain: f64) -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Bureau", Some(output_type), Some("dev-7762"))
        .unwrap();
    repo.update_volume(id, gain * 100.0).unwrap();
    (backend.clone(), repo.get(id).unwrap().unwrap())
}

/// Le cas du signalement : sortie locale Windows (avec sonde), curseur à 50,
/// donc gain 0,0316. Le panneau dit 50 %, pas 3 %.
#[test]
fn sortie_locale_avec_sonde_curseur_50_affiche_50_et_non_3_7762() {
    let (backend, zone) = zone_au_gain("local", gain_perceptif(0.5));
    assert!(
        (zone.volume / 100.0 - 0.031_622_8).abs() < 1e-6,
        "{}",
        zone.volume
    );
    let mut ps = wav24_en_lecture();
    ps.output_signal_path = Some(sonde_volume_applique());

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "WASAPI", None).unwrap();

    assert_eq!(
        description_volume(&sp).as_deref(),
        Some("Volume logiciel 50%"),
        "#7762 : le chemin du signal doit afficher la position du curseur db60 (50), \
         pas le gain 0,0316 lu comme un pourcentage : {sp}"
    );
}

/// Même zone, sans sonde (macOS, Linux) : même règle.
#[test]
fn sortie_locale_sans_sonde_curseur_50_affiche_50_7762() {
    let (backend, zone) = zone_au_gain("local", gain_perceptif(0.5));
    let ps = wav24_en_lecture();

    let sp = build_signal_path(&ps, &zone, &backend, Some("DAC"), "none", None).unwrap();

    assert_eq!(
        description_volume(&sp).as_deref(),
        Some("Volume 50%"),
        "#7762 : sans sonde, le chemin du signal doit afficher la position db60 : {sp}"
    );
}

/// Témoin de non-régression : une zone à curseur linéaire (appareil réseau)
/// affiche toujours son gain, qui EST son curseur.
#[test]
fn zone_lineaire_affiche_toujours_son_curseur_7762() {
    let (backend, zone) = zone_au_gain("dlna", 0.5);
    let ps = wav24_en_lecture();

    let sp = build_signal_path(&ps, &zone, &backend, Some("Salon"), "none", None).unwrap();

    assert_eq!(
        description_volume(&sp).as_deref(),
        Some("Volume 50%"),
        "{sp}"
    );
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
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

/// De bout en bout par le routeur : le curseur réglé à 50 sur une sortie
/// locale factice se relit à 50 (GET /zones, réglage en base, chemin du
/// signal), et la sortie ne reçoit le gain 0,0316 qu'UNE fois, jamais
/// 0,0316 repassé dans la loi (0,001).
#[tokio::test]
async fn curseur_50_regle_relu_partout_a_50_et_gain_applique_une_fois_7762() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo.create("Bureau", Some("local"), Some(LOCALE)).unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(LOCALE, "Bureau").with_type("local"),
    ));
    // Départ plus bas que la cible : la garde de hausse n'est pas l'objet ici.
    repo.update_volume(id, 1.0).unwrap();
    state.playback.set_volume(id, 0.01).await;

    // Le client à jour envoie le gain de la position 50.
    let gain_50 = EchelleDeVolume::Perceptive.gain(0.5);
    let (statut, rendu) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": gain_50, "confirm_increase": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rendu}");
    assert_eq!(rendu["volume_curve"], "db60", "{rendu}");

    // GET /zones/{id} : le gain, et la position 50 qu'en tire le client.
    let (statut, zone_json) = appeler(&app, "GET", &format!("/api/v1/zones/{id}"), json!({})).await;
    assert_eq!(statut, StatusCode::OK, "{zone_json}");
    assert_eq!(zone_json["volume_curve"], "db60", "{zone_json}");
    let gain = zone_json["volume"].as_f64().unwrap();
    assert!((gain - 0.031_622_8).abs() < 1e-6, "{zone_json}");
    let position = EchelleDeVolume::Perceptive.position(gain);
    assert_eq!((position * 100.0).round() as i32, 50, "{zone_json}");

    // Le réglage en base garde le gain, une seule fois pondéré.
    let zone = repo.get(id).unwrap().unwrap();
    assert!((zone.volume - 3.162_28).abs() < 1e-4, "{}", zone.volume);

    // La sortie factice n'a reçu que le gain 0,0316, jamais 0,001.
    let recus = {
        let outputs = state.outputs.lock().await;
        let out = outputs.get(LOCALE).unwrap();
        let guard = out.lock().await;
        guard
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .volume_calls()
            .await
    };
    assert!(!recus.is_empty(), "la sortie n'a reçu aucun volume");
    for v in &recus {
        assert!(
            (v - gain_50).abs() < 1e-6,
            "#7762 : la sortie a reçu {v} au lieu du gain {gain_50} (reçus : {recus:?})"
        );
    }

    // Le chemin du signal, lu sur la zone relue en base, dit 50 %.
    let mut ps = wav24_en_lecture();
    ps.output_signal_path = Some(sonde_volume_applique());
    let sp = build_signal_path(&ps, &zone, &state.backend, Some("Bureau"), "WASAPI", None).unwrap();
    assert_eq!(
        description_volume(&sp).as_deref(),
        Some("Volume logiciel 50%"),
        "#7762 : curseur réglé à 50, chemin du signal relu : {sp}"
    );
}
