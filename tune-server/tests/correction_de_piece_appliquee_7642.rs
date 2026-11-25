//! #7642 — un profil de correction de pièce (ou de calibration de DAC)
//! appliqué par sa route atteint-il VRAIMENT le DSP de la zone ?
//!
//! Les deux routes `apply` écrivaient sous `zone_{id}_eq_profile` un JSON
//! fait main, sans les champs obligatoires d'`EqProfile` (`listening`,
//! `room_size`, `speaker_placement`, `bass/mid/treble_gain_db`). La lecture
//! de la chaîne audio (`eq_profile_configure_with`) le désérialise par
//! `.ok()?` : échec silencieux, aucun égaliseur — pendant que la réponse
//! annonçait `applied: true`.
//!
//! Le témoin passe par le routeur réel et lit deux choses :
//! - `GET /zones/{id}/dsp`, ce que l'écran affiche (`eq_profile`) ;
//! - `PlaybackOrchestrator::gain_moyen_du_dsp_de_zone`, qui bâtit
//!   l'égaliseur par le même chargeur que la lecture (`load_eq_processor`).
//!
//! Aucun son n'est joué.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const ZONE: i64 = 1;

async fn serveur() -> (tune_server::state::AppState, axum::Router) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // L'égaliseur est un greffon facultatif : la chaîne ne le lit que s'il
    // est installé (`premium_plugins::enabled`).
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_equalizer_installed", "true")
        .unwrap();
    state.license.set_account_premium(true, None).await;
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Ce que la zone porte effectivement, vu de l'écran et vu de la chaîne.
async fn dsp_effectif(state: &tune_server::state::AppState, app: &axum::Router) -> (Value, f64) {
    let (status, dsp) = appel(app, "GET", &format!("/api/v1/zones/{ZONE}/dsp"), None).await;
    assert_eq!(status, StatusCode::OK, "{dsp}");
    let (gain_eq_db, _) = state.orchestrator.gain_moyen_du_dsp_de_zone(ZONE);
    (dsp["eq_profile"].clone(), gain_eq_db)
}

fn verifier_applique(eq: &Value, gain_eq_db: f64, freq: f64, gain: f64) {
    assert_eq!(
        eq["enabled"], true,
        "#7642 : le profil écrit par la route n'est pas relu comme un EqProfile actif — \
         la zone n'a AUCUN égaliseur malgré `applied: true` : {eq}"
    );
    let bandes = eq["bands"].as_array().expect("bandes");
    assert_eq!(bandes.len(), 1, "{eq}");
    assert_eq!(bandes[0]["freq"].as_f64(), Some(freq), "{eq}");
    assert_eq!(bandes[0]["gain"].as_f64(), Some(gain), "{eq}");
    assert_eq!(bandes[0]["type"], "peak", "{eq}");
    assert!(
        gain_eq_db < -0.01,
        "#7642 : la chaîne audio ne bâtit aucun égaliseur pour la zone \
         (gain moyen {gain_eq_db} dB, attendu < 0 pour une coupe de {gain} dB)"
    );
}

#[tokio::test]
async fn un_profil_de_correction_de_piece_applique_atteint_le_dsp_de_la_zone() {
    let (state, app) = serveur().await;

    // Contre-épreuve de départ : rien d'actif avant l'application.
    let (eq, gain) = dsp_effectif(&state, &app).await;
    assert_eq!(eq["enabled"], false, "{eq}");
    assert_eq!(gain, 0.0);

    let (status, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/profiles/{ZONE}"),
        Some(json!({
            "name": "Salon",
            "filters": [{"frequency_hz": 63.0, "gain_db": -8.0, "q_factor": 1.0, "filter_type": "peaking"}],
        })),
    )
    .await;
    assert!(status.is_success(), "{status} {corps}");

    let (status, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/profiles/{ZONE}/apply"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["applied"], true, "{corps}");

    let (eq, gain) = dsp_effectif(&state, &app).await;
    verifier_applique(&eq, gain, 63.0, -8.0);
}

#[tokio::test]
async fn une_calibration_de_dac_appliquee_atteint_le_dsp_de_la_zone() {
    let (state, app) = serveur().await;

    let (status, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/dac-calibration/zones/{ZONE}/apply"),
        Some(json!({
            "profile": {
                "slug": "essai-dac",
                "manufacturer": "Essai",
                "model": "DAC",
                "description": "témoin #7642",
                "corrections": [{"frequency_hz": 8000.0, "gain_db": -6.0, "q_factor": 1.0, "filter_type": "peaking"}],
                "contributed_by": null,
                "measurement_url": null,
                "created_at": "2026-11-01T00:00:00Z",
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{corps}");
    assert_eq!(corps["applied"], true, "{corps}");

    let (eq, gain) = dsp_effectif(&state, &app).await;
    verifier_applique(&eq, gain, 8000.0, -6.0);
}

/// Le profil écrit se relit comme un `EqProfile` complet : les sept champs
/// obligatoires y sont, avec les valeurs par défaut explicites.
#[tokio::test]
async fn le_profil_ecrit_porte_tous_les_champs_obligatoires() {
    let (state, app) = serveur().await;
    appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/profiles/{ZONE}"),
        Some(json!({"name": "Salon", "filters": [{"frequency_hz": 100.0, "gain_db": -3.0}]})),
    )
    .await;
    appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/profiles/{ZONE}/apply"),
        None,
    )
    .await;

    let brut = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("zone_{ZONE}_eq_profile"))
        .unwrap()
        .expect("profil écrit");
    let v: Value = serde_json::from_str(&brut).unwrap();
    for champ in [
        "enabled",
        "listening",
        "room_size",
        "speaker_placement",
        "bass_gain_db",
        "mid_gain_db",
        "treble_gain_db",
    ] {
        assert!(v.get(champ).is_some(), "champ `{champ}` absent : {brut}");
    }
    let p: tune_core::audio::eq::EqProfile =
        serde_json::from_str(&brut).expect("le profil écrit doit se relire comme EqProfile");
    assert!(p.enabled);
    assert_eq!(p.bands.len(), 1);
}

/// D1 (TESS, 18/11 soir) : un profil posé à la main par l'API enregistre son
/// origine, toujours « mesurée » (Premium), même s'il porte le nom d'une
/// estimation C5 ou que le corps se dit « estimation ».
#[tokio::test]
async fn un_profil_pose_par_l_api_enregistre_une_origine_mesuree() {
    let (state, app) = serveur().await;
    let (status, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/room-correction/profiles/{ZONE}"),
        Some(json!({
            "name": "Estimation 9",
            "origin": "estimation",
            "filters": [{"frequency_hz": 63.0, "gain_db": -8.0}],
        })),
    )
    .await;
    assert!(status.is_success(), "{status} {corps}");
    assert_eq!(corps["origin"], "measured", "{corps}");
    let brut = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("room_profile_{ZONE}"))
        .unwrap()
        .expect("profil enregistré");
    let v: Value = serde_json::from_str(&brut).unwrap();
    assert_eq!(v["origin"], "measured", "{brut}");
    assert_eq!(
        tune_core::tess::migration::origine_de_la_piece(&v),
        "measured"
    );
}

/// rc14 (retours rc13.90) — après `apply`, la chaîne TESS de la zone porte
/// « Correction de pièce » ET la garde quand un nouveau profil de pièce est
/// enregistré sans être appliqué (nouvelle mesure). Elle était relue à la
/// volée d'après `room_profile_{id}` et devenait « Égaliseur paramétrique ».
#[tokio::test]
async fn la_correction_appliquee_reste_dans_la_chaine_tess_apres_un_nouveau_profil() {
    let (state, app) = serveur().await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    let profil = |gain: f64| {
        json!({
            "name": "Salon",
            "filters": [{"frequency_hz": 63.0, "gain_db": gain, "q_factor": 1.0, "filter_type": "peaking"}],
        })
    };
    let facades = |v: &Value| -> Vec<(String, bool)> {
        v["chain"]["plugins"]
            .as_array()
            .expect("chaîne")
            .iter()
            .map(|m| (m["id"].as_str().unwrap().to_owned(), m["enabled"] == true))
            .collect()
    };

    let chemin = format!("/api/v1/room-correction/profiles/{zone}");
    let (status, corps) = appel(&app, "POST", &chemin, Some(profil(-8.0))).await;
    assert!(status.is_success(), "{status} {corps}");
    let (status, corps) = appel(&app, "POST", &format!("{chemin}/apply"), None).await;
    assert_eq!(status, StatusCode::OK, "{corps}");

    let tess = format!("/api/v1/zones/{zone}/tess");
    let (status, v) = appel(&app, "GET", &tess, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        facades(&v),
        vec![("room-correction".to_owned(), true)],
        "{v}"
    );

    // Nouvelle mesure enregistrée, pas encore appliquée.
    let (status, corps) = appel(&app, "POST", &chemin, Some(profil(-3.0))).await;
    assert!(status.is_success(), "{status} {corps}");
    let (status, v) = appel(&app, "GET", &tess, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        facades(&v),
        vec![("room-correction".to_owned(), true)],
        "la correction appliquée a changé de façade : {v}"
    );
}

/// rc14 (égaliseurs en série, suite de #9532) — `apply` sur une série
/// `[graphique, correction de pièce]` met la correction à jour et garde le
/// graphique. L'ancienne écriture directe d'`eq_profile` remplaçait le
/// graphique (principal) et supprimait la correction comme doublon.
#[tokio::test]
async fn appliquer_un_profil_garde_le_graphique_de_la_serie() {
    let (state, app) = serveur().await;
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    let tess = format!("/api/v1/zones/{zone}/tess");
    let (status, v) = appel(&app, "GET", &tess, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let gains: Vec<Value> = (0..10)
        .map(|i| json!(if i == 4 { 3.0 } else { 0.0 }))
        .collect();
    let grille = [
        31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
    ];
    let bandes: Vec<Value> = grille
        .iter()
        .zip(&gains)
        .map(|(f, g)| json!({"freq": f, "gain": g, "q": 1.0, "type": "peak"}))
        .collect();
    let mut chaine = v["chain"].clone();
    chaine["plugins"] = json!([
        {"id": "equalizer-graphic", "enabled": true, "settings": {
            "listening": "speakers", "room_size": "medium", "speaker_placement": "free_standing",
            "bass_gain_db": 0.0, "mid_gain_db": 0.0, "treble_gain_db": 0.0, "bands": bandes}},
        {"id": "room-correction", "enabled": true, "settings": null},
    ]);
    let (status, v) = appel(&app, "PUT", &tess, Some(chaine)).await;
    assert_eq!(status, StatusCode::OK, "{v}");

    let chemin = format!("/api/v1/room-correction/profiles/{zone}");
    let corps = json!({
        "name": "Salon",
        "filters": [{"frequency_hz": 63.0, "gain_db": -8.0, "q_factor": 1.0, "filter_type": "peaking"}],
    });
    let (status, c) = appel(&app, "POST", &chemin, Some(corps)).await;
    assert!(status.is_success(), "{status} {c}");
    let (status, c) = appel(&app, "POST", &format!("{chemin}/apply"), None).await;
    assert_eq!(status, StatusCode::OK, "{c}");

    let (status, v) = appel(&app, "GET", &tess, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let plugins = v["chain"]["plugins"].as_array().expect("chaîne");
    let ids: Vec<(&str, bool)> = plugins
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["enabled"] == true))
        .collect();
    assert_eq!(
        ids,
        vec![("equalizer-graphic", true), ("room-correction", true)],
        "le graphique de la série a été perdu : {v}"
    );
    assert_eq!(
        plugins[1]["settings"]["bands"][0]["freq"],
        json!(63.0),
        "{v}"
    );
}
