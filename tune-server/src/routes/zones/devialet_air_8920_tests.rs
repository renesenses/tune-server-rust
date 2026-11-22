//! #8920 — retours d'un testeur Devialet Expert, par le routeur public, sur
//! une sortie factice : rien ne sort sur un vrai ampli. Chaque refus a sa
//! contre-épreuve sur une zone DLNA.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const AMPLI: &str = "devialet-air:02:00:5e:00:00:09";
const SALON: &str = "dlna-salon-8920";

async fn serveur() -> (axum::Router, crate::state::AppState, i64, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let air = repo
        .create("Expert", Some("devialet_air"), Some(AMPLI))
        .unwrap();
    let salon = repo.create("Salon", Some("dlna"), Some(SALON)).unwrap();
    let mut outputs = state.outputs.lock().await;
    outputs.register(Box::new(
        MockOutput::new(AMPLI, "Expert").with_type("devialet_air"),
    ));
    outputs.register(Box::new(MockOutput::new(SALON, "Salon").with_type("dlna")));
    drop(outputs);
    // −52 dB sur la course par défaut (−80 … −10 dB).
    repo.update_volume(air, 40.0).unwrap();
    state.playback.set_volume(air, 0.4).await;
    repo.update_volume(salon, 40.0).unwrap();
    state.playback.set_volume(salon, 0.4).await;
    (routeur, state, air, salon)
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
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

async fn recus(state: &crate::state::AppState, appareil: &str) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let out = outputs.get(appareil).unwrap();
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .volume_calls()
        .await
}

/// Volume fixe, cadence au-delà de 192 kHz, DSD natif ou « auto » : refusés
/// sur Devialet AIR, rien n'est écrit ni envoyé. Contre-épreuve DLNA.
#[tokio::test]
async fn air_8920_patch_refuse_volume_fixe_cadence_et_dsd() {
    let (app, state, air, salon) = serveur().await;
    for (corps, champ) in [
        (
            json!({ "fixed_volume": true, "confirm_full_volume": true }),
            "fixed_volume",
        ),
        (json!({ "max_sample_rate": 384000 }), "max_sample_rate"),
        (json!({ "dsd_mode": "native" }), "dsd_mode"),
        (json!({ "dsd_mode": "auto" }), "dsd_mode"),
    ] {
        let (statut, r) = appeler(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{air}"),
            Some(corps.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps} : {r}");
        assert!(r.to_string().contains(champ), "{r}");
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(air)
        .unwrap()
        .unwrap();
    assert!(!zone.fixed_volume);
    assert_eq!(zone.max_sample_rate, None);
    assert!(recus(&state, AMPLI).await.is_empty(), "rien n'est parti");
    for corps in [
        json!({ "max_sample_rate": 192000 }),
        json!({ "dsd_mode": "dop" }),
        json!({ "dsd_mode": "pcm" }),
        json!({ "fixed_volume": false }),
    ] {
        let (statut, r) = appeler(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{air}"),
            Some(corps.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps} : {r}");
    }
    assert_eq!(
        ZoneRepo::with_backend(state.backend.clone())
            .get(air)
            .unwrap()
            .unwrap()
            .max_sample_rate,
        Some(192_000)
    );
    // Contre-épreuve : hors Devialet AIR, ces valeurs passent.
    for corps in [
        json!({ "max_sample_rate": 384000 }),
        json!({ "dsd_mode": "native" }),
    ] {
        let (statut, r) = appeler(
            &app,
            "PATCH",
            &format!("/api/v1/zones/{salon}"),
            Some(corps.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps} : {r}");
    }
}

/// « Plein volume en PURE » refusé sur Devialet AIR, rien ne part ;
/// contre-épreuve DLNA : la même demande passe et commande 100 %.
#[tokio::test]
async fn air_8920_plein_volume_en_pure_refuse() {
    let (app, state, air, salon) = serveur().await;
    let corps = json!({ "enabled": true, "lock_volume": true, "confirm_full_volume": true });
    let (statut, r) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{air}/audiophile"),
        Some(corps.clone()),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{r}");
    assert_eq!(r["error"], "full_volume_unavailable_devialet_air");
    assert!(recus(&state, AMPLI).await.is_empty());
    // Le verrou GLOBAL armé : PURE s'allume sans plein volume.
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(tune_core::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    let (statut, r) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{air}/audiophile"),
        Some(json!({ "enabled": true, "confirm_full_volume": true })),
    )
    .await;
    assert!(statut.is_success(), "{r}");
    assert!(
        recus(&state, AMPLI).await.iter().all(|v| *v < 1.0),
        "{:?}",
        recus(&state, AMPLI).await
    );
    // Contre-épreuve.
    let (statut, r) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{salon}/audiophile"),
        Some(corps),
    )
    .await;
    assert!(statut.is_success(), "{r}");
    assert_eq!(recus(&state, SALON).await.last().copied(), Some(1.0));
}

/// Les boutons ±1 dB : un dB d'ampli par pas, au-delà du confort, jamais
/// au-delà de la sécurité ; en butée, rien ne part et la réponse le dit.
#[tokio::test]
async fn air_8920_pas_d_un_db_borne_par_la_securite() {
    let (app, state, air, _) = serveur().await;
    let (statut, r) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume-limits"),
        Some(json!({ "safety_device_db": -50.0, "comfort_device_db": -60.0 })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{r}");
    let avant = recus(&state, AMPLI).await.len();
    let mut niveaux = Vec::new();
    for _ in 0..4 {
        let (statut, r) = appeler(
            &app,
            "POST",
            &format!("/api/v1/zones/{air}/volume/step"),
            Some(json!({ "direction": "up", "step_db": 1.0 })),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{r}");
        niveaux.push((
            r["device_volume_db"].as_f64(),
            r["sent"].as_bool(),
            r["limited"].as_bool(),
        ));
    }
    assert_eq!(
        niveaux,
        vec![
            (Some(-51.0), Some(true), Some(false)),
            (Some(-50.0), Some(true), Some(false)),
            (Some(-50.0), Some(false), Some(true)),
            (Some(-50.0), Some(false), Some(true)),
        ]
    );
    let envoyes = recus(&state, AMPLI).await;
    assert_eq!(envoyes.len() - avant, 2, "{envoyes:?}");
    let securite = 30.0 / 70.0;
    assert!(envoyes.iter().all(|v| *v <= securite + 1e-9), "{envoyes:?}");
    // Le − redescend d'un dB.
    let (_, r) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{air}/volume/step"),
        Some(json!({ "direction": "down", "step_db": 1.0 })),
    )
    .await;
    assert_eq!(r["device_volume_db"], -51.0, "{r}");
    // Contre-épreuve : le curseur (consigne absolue) ne passe pas le confort.
    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume"),
        Some(json!({ "device_volume_db": -45.0 })),
    )
    .await;
    assert!(statut.is_client_error() || statut.is_success());
    assert!(
        recus(&state, AMPLI)
            .await
            .iter()
            .all(|v| *v <= securite + 1e-9),
        "{:?}",
        recus(&state, AMPLI).await
    );
}

/// Le nom de l'appareil au chemin du signal : la zone et l'IP.
#[test]
fn air_8920_libelle_du_renderer() {
    use super::signal_path::libelle_air;
    assert_eq!(
        libelle_air("Salon Expert", Some("192.168.1.42".parse().unwrap())),
        "Salon Expert (192.168.1.42)"
    );
    assert_eq!(libelle_air("Salon Expert", None), "Salon Expert");
}
