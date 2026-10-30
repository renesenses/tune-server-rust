//! #6918 — le volume en dB d'APPAREIL d'une zone Devialet AIR, par le
//! routeur public, sur une sortie factice : rien ne sort sur un vrai ampli.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const AMPLI: &str = "devialet-air:02:00:5e:00:00:01";
const SALON: &str = "dlna-salon-6918";

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
    state.playback.set_volume(air, 0.4).await;
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

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// La consigne en dB d'appareil suit la course réglée ; au-dessus de la
/// borne haute (et donc du plafond), elle est refusée et rien ne part.
#[tokio::test]
async fn le_volume_se_regle_et_se_lit_en_db_d_appareil_6918() {
    let (app, state, air, _) = serveur().await;
    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/devialet-air/reglages",
        Some(json!({ "volume_min_db": -70, "volume_max_db": -20 })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume"),
        Some(json!({ "device_volume_db": -45.0 })),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT, "{corps}");
    assert!(proche(*recus(&state, AMPLI).await.last().unwrap(), 0.5));

    let (statut, zone) = appeler(&app, "GET", &format!("/api/v1/zones/{air}"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(zone["device_volume_db"], -45.0, "{zone}");
    assert_eq!(zone["device_volume_range"]["min_db"], -70.0);
    assert_eq!(zone["device_volume_range"]["max_db"], -20.0);
    assert_eq!(zone["device_volume_range"]["ceiling_db"], -10.0);

    let (_, statut_zone) = appeler(&app, "GET", &format!("/api/v1/zones/{air}/status"), None).await;
    assert_eq!(statut_zone["device_volume_db"], -45.0, "{statut_zone}");

    let envoyes = recus(&state, AMPLI).await.len();
    for refusee in [
        json!({ "device_volume_db": -19.5 }),
        json!({ "device_volume_db": -10.0 }),
        json!({ "device_volume_db": 0.0 }),
        json!({ "device_volume_db": 6.0 }),
        json!({ "device_volume_db": -75.0 }),
        json!({ "device_volume_db": -30.0, "volume": 0.5 }),
        json!({ "device_volume_db": -30.0, "volume_db": -6.0 }),
    ] {
        let (statut, corps) = appeler(
            &app,
            "PUT",
            &format!("/api/v1/zones/{air}/volume"),
            Some(refusee.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{refusee} : {corps}");
        assert_eq!(corps["error"], "invalid_device_volume_db", "{corps}");
    }
    assert_eq!(
        recus(&state, AMPLI).await.len(),
        envoyes,
        "une demande refusée n'envoie rien"
    );
}

/// La limite de sécurité #6541 se pose en dB d'appareil et borne la
/// consigne en dB d'appareil.
#[tokio::test]
async fn la_securite_se_pose_en_db_d_appareil_et_borne_6918() {
    let (app, state, air, _) = serveur().await;
    // Course par défaut −80..−10 : −24 dB = 80 %.
    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume-limits"),
        Some(json!({ "safety_device_db": -24.0 })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(proche(corps["volume_limit_safety"].as_f64().unwrap(), 0.8));
    assert_eq!(corps["volume_limit_safety_device_db"], -24.0, "{corps}");

    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume"),
        Some(json!({ "device_volume_db": -17.0 })),
    )
    .await;
    assert!(statut.is_success(), "{corps}");
    let envoye = *recus(&state, AMPLI).await.last().unwrap();
    assert!(envoye <= 0.8 + 1e-9, "la sécurité borne : {envoye}");

    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{air}/volume-limits"),
        Some(json!({ "safety_device_db": 0.0 })),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
}

/// Une sortie sans course en dB : les champs sont présents et `null`, et une
/// demande en dB d'appareil est refusée.
#[tokio::test]
async fn une_sortie_sans_course_en_db_publie_null_et_refuse_6918() {
    let (app, state, _, salon) = serveur().await;
    let (_, zone) = appeler(&app, "GET", &format!("/api/v1/zones/{salon}"), None).await;
    for cle in [
        "device_volume_db",
        "device_volume_range",
        "volume_limit_safety_device_db",
        "volume_limit_comfort_device_db",
    ] {
        assert!(zone.get(cle).is_some_and(Value::is_null), "{cle} : {zone}");
    }
    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{salon}/volume"),
        Some(json!({ "device_volume_db": -30.0 })),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert!(recus(&state, SALON).await.is_empty());
}

/// Changer la course garde le niveau en dB de la zone et n'envoie rien.
#[tokio::test]
async fn changer_la_course_garde_le_niveau_et_n_envoie_rien_6918() {
    let (app, state, air, _) = serveur().await;
    // 40 % sur −80..−10 = −52 dB.
    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/devialet-air/reglages",
        Some(json!({ "volume_min_db": -60, "volume_max_db": -20 })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(recus(&state, AMPLI).await.is_empty(), "rien n'est envoyé");
    let v = state.playback.get_state(air).await.volume;
    assert!(proche(v, 0.2), "−52 dB sur −60..−20 = 20 % : {v}");
}

/// Le relevé haut s'affiche séparément : ni une lecture ni l'évènement ne commande.
#[tokio::test]
async fn l_observation_air_est_distincte_de_la_consigne_sur_les_routes_6912() {
    let (app, state, air, _) = serveur().await;
    ZoneRepo::with_backend(state.backend.clone())
        .update_volume(air, 40.0)
        .unwrap();
    state
        .playback
        .noter_observation_volume_appareil(
            air,
            0.4,
            false,
            Some(tune_core::playback::ObservationVolumeAppareil {
                device_id: AMPLI.into(),
                volume: 0.8,
                volume_db: -24.0,
                muted: false,
            }),
        )
        .await;
    for chemin in [
        format!("/api/v1/zones/{air}"),
        format!("/api/v1/zones/{air}/status"),
    ] {
        let (statut, zone) = appeler(&app, "GET", &chemin, None).await;
        assert_eq!(statut, StatusCode::OK, "{zone}");
        assert_eq!(
            zone["volume"], 0.4,
            "le relevé AIR ne doit pas remplacer la consigne API"
        );
        assert_eq!(zone["device_volume_db"], -52.0);
        assert_eq!(
            zone["device_volume_observation"]["volume_db"], -24.0,
            "les dB observés doivent être publiés séparément de la consigne"
        );
        assert_eq!(zone["device_volume_observation"]["device_id"], AMPLI);
    }
    let (_, liste) = appeler(&app, "GET", "/api/v1/zones", None).await;
    let zone = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|z| z["id"] == air)
        .unwrap();
    assert_eq!(
        zone["device_volume_observation"]["volume_db"], -24.0,
        "la liste de zones doit aussi porter l'observation AIR"
    );
    assert_eq!(zone["volume"], 0.4);
    assert_eq!(
        ZoneRepo::with_backend(state.backend.clone())
            .get(air)
            .unwrap()
            .unwrap()
            .volume,
        40.0
    );
    assert!(
        recus(&state, AMPLI).await.is_empty(),
        "lire une observation AIR ne doit envoyer aucun volume"
    );
}

#[tokio::test]
async fn retirer_l_annonce_air_retire_aussi_son_observation_api_6912() {
    let (app, state, air, _) = serveur().await;
    ZoneRepo::with_backend(state.backend.clone())
        .update_volume(air, 40.0)
        .unwrap();
    state
        .playback
        .noter_observation_volume_appareil(
            air,
            0.4,
            false,
            Some(tune_core::playback::ObservationVolumeAppareil {
                device_id: AMPLI.into(),
                volume: 1.0,
                volume_db: 0.0,
                muted: false,
            }),
        )
        .await;
    state
        .playback
        .noter_observation_volume_appareil(air, 0.4, false, None)
        .await;
    for chemin in [
        format!("/api/v1/zones/{air}"),
        format!("/api/v1/zones/{air}/status"),
    ] {
        let (statut, zone) = appeler(&app, "GET", &chemin, None).await;
        assert_eq!(statut, StatusCode::OK);
        assert!(
            zone.get("device_volume_observation")
                .is_some_and(Value::is_null),
            "une annonce absente ne doit plus afficher un ancien relevé AIR"
        );
        assert_eq!(zone["volume"], 0.4);
    }
    assert!(recus(&state, AMPLI).await.is_empty());
}
