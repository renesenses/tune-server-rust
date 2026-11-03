//! #6721 — la garde contre les montées brusques, de bout en bout par le
//! routeur public, sur une sortie factice : ce que l'appareil reçoit, ce que
//! le client lit dans la réponse, et comment il confirme.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::audio::garde_de_hausse::SEUIL_DE_HAUSSE_DB;
use tune_core::audio::volume_scale::PLAGE_PERCEPTIVE_DB;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna-marantz-6721";
const POINTS_DU_SEUIL: f64 = SEUIL_DE_HAUSSE_DB / PLAGE_PERCEPTIVE_DB;

/// Un serveur neuf (c'est aussi un redémarrage), une zone DLNA enregistrée à
/// `depart` en base. La mémoire n'est PAS ensemencée : les appelants qui
/// veulent un démarrage réel appellent `restore_zone_volumes`.
async fn serveur(depart: f64) -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = repo
        .create("Marantz NR1200", Some("dlna"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, depart * 100.0).unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Marantz NR1200").with_type("dlna"),
    ));
    (routeur, state, zone_id)
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

async fn recus(state: &crate::state::AppState) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let out = outputs.get(APPAREIL).unwrap();
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

/// `PUT /zones/{id}/volume` (curseur web, remote Android) : 10 % → 100 %
/// s'arrête au plafond, la réponse le dit (200 au lieu de 204), et la même
/// requête avec `confirm_increase` passe.
#[tokio::test]
async fn put_volume_plafonne_puis_confirme_6721() {
    let (app, state, id) = serveur(0.10).await;
    crate::startup::restore_zone_volumes(&state).await;
    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0 }),
    )
    .await;
    let plafond = 0.10 + POINTS_DU_SEUIL;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["increase_capped"], true, "{corps}");
    assert!(
        proche(corps["volume"].as_f64().unwrap(), plafond),
        "{corps}"
    );
    assert_eq!(corps["requested_volume"], 1.0);
    assert_eq!(corps["confirm_field"], "confirm_increase");
    assert_eq!(corps["max_increase_db"], SEUIL_DE_HAUSSE_DB);
    let recu = recus(&state).await;
    assert!(
        recu.iter().all(|v| *v <= plafond + 1e-9),
        "#6721 : PUT à 100 % d'une zone à 10 % a fait recevoir {recu:?}"
    );

    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0, "confirm_increase": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT, "{corps}");
    assert_eq!(*recus(&state).await.last().unwrap(), 1.0);
}

/// Une consigne ordinaire : 204, sans corps, comme avant.
#[tokio::test]
async fn put_volume_ordinaire_inchange_6721() {
    let (app, state, id) = serveur(0.30).await;
    crate::startup::restore_zone_volumes(&state).await;
    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 0.35 }),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert_eq!(recus(&state).await, vec![0.35]);
}

/// `POST /zones/{id}/volume` (route `playback::set_volume`) : la réponse
/// rend le gain RETENU.
#[tokio::test]
async fn post_playback_volume_plafonne_6721() {
    let (app, state, id) = serveur(0.10).await;
    crate::startup::restore_zone_volumes(&state).await;
    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume_db": 0.0 }),
    )
    .await;
    let plafond = 0.10 + POINTS_DU_SEUIL;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["increase_capped"], true, "{corps}");
    assert!(
        proche(corps["volume"].as_f64().unwrap(), plafond),
        "{corps}"
    );
    assert!(*recus(&state).await.last().unwrap() <= plafond + 1e-9);
}

/// `PATCH /zones/{id}` porte aussi le volume : gardé.
#[tokio::test]
async fn patch_zone_volume_plafonne_6721() {
    let (app, state, id) = serveur(0.10).await;
    crate::startup::restore_zone_volumes(&state).await;
    let (statut, corps) = appeler(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "volume": 100 }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    let recu = recus(&state).await;
    assert!(
        recu.iter().all(|v| *v <= 0.10 + POINTS_DU_SEUIL + 1e-9),
        "#6721 : PATCH volume 100 a fait recevoir {recu:?}"
    );
}

/// Boutons + / − et raccourci clavier du client web (`POST
/// /zones/{id}/volume/step`) : un pas ordinaire passe, le plus grand pas
/// permis (25 points) est retenu au seuil.
#[tokio::test]
async fn pas_de_volume_garde_6721() {
    let (app, state, id) = serveur(0.10).await;
    crate::startup::restore_zone_volumes(&state).await;
    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume/step"),
        json!({ "direction": "up", "step": 0.05 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(proche(*recus(&state).await.last().unwrap(), 0.15));
    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume/step"),
        // Le plus grand pas permis (25 points) dépasse le seuil (16,7).
        json!({ "direction": "up", "step": 0.25 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let recu = recus(&state).await;
    assert!(
        *recu.last().unwrap() <= 0.15 + POINTS_DU_SEUIL + 1e-9,
        "#6721 : un pas de 25 points est passé en entier : {recu:?}"
    );
}

/// Reprise après redémarrage : la mémoire est rechargée depuis la base par
/// `restore_zone_volumes`, et la garde part de CE volume (10 %), pas d'une
/// valeur par défaut.
#[tokio::test]
async fn apres_un_redemarrage_la_garde_part_du_volume_enregistre_6721() {
    let (app, state, id) = serveur(0.10).await;
    crate::startup::restore_zone_volumes(&state).await;
    appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0 }),
    )
    .await;
    let recu = recus(&state).await;
    assert!(
        recu.iter().all(|v| *v <= 0.10 + POINTS_DU_SEUIL + 1e-9),
        "#6721 : après redémarrage, la zone à 10 % a reçu {recu:?}"
    );
}
