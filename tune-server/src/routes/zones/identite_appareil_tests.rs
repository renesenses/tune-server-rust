//! #3660 — le vide FORCÉ sur l'identité d'appareil d'une zone.
//!
//! Le fait mesuré sur le .18 le 08/09/2026 : `PATCH /zones/{id}` traite la
//! chaîne vide comme « efface l'override », et l'identité **détectée** par la
//! découverte UPnP reprend aussitôt la main. Treize zones sur quatorze n'ont
//! que cette détection — dont un `detected_model = "AV Renderer Device"` qui
//! ne désigne aucun modèle. Il n'existait aucune valeur pour « aucun
//! appareil », distincte de « pas d'override ».

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

/// L'Eversolo du ticket : une marque criée en capitales et un « modèle » qui
/// n'en est pas un.
fn eversolo() -> DiscoveredDevice {
    let mut d = DiscoveredDevice::new(
        "uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE".into(),
        "Salon".into(),
        OutputType::Dlna,
        "192.168.1.17".into(),
        49152,
    );
    d.manufacturer = Some("EVERSOLO".into());
    d.model = Some("AV Renderer Device".into());
    d
}

fn identite(
    state: &crate::state::AppState,
    zone_id: i64,
    detecte: Option<&DiscoveredDevice>,
) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    crate::routes::zones::inject_device_identity(&mut obj, &state.backend, zone_id, None, detecte);
    obj
}

/// Ce qui manquait : un état où l'appareil détecté N'EST PLUS l'identité de la
/// zone — et qui ne se confond pas avec « aucun override ».
#[test]
fn le_vide_force_retire_la_detection_de_l_identite_de_la_zone() {
    let (_, state) = serveur();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create(
            "Salon",
            Some("dlna"),
            Some("uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE"),
        )
        .unwrap();
    let dev = eversolo();

    // Avant : la détection EST l'identité publiée de la zone.
    let avant = identite(&state, zone_id, Some(&dev));
    assert_eq!(
        avant["detected_manufacturer"],
        json!("EVERSOLO"),
        "{avant:#?}"
    );
    assert_eq!(
        avant["detected_model"],
        json!("AV Renderer Device"),
        "{avant:#?}"
    );
    assert_eq!(
        avant["identite_appareil_effacee"],
        json!(false),
        "{avant:#?}"
    );

    SettingsRepo::with_backend(state.backend.clone())
        .set(&crate::routes::zones::cle_identite_effacee(zone_id), "true")
        .unwrap();

    // Après : récusée, et le drapeau le dit — sans quoi un écran ne pourrait
    // pas distinguer « rien détecté » de « détection récusée ».
    let apres = identite(&state, zone_id, Some(&dev));
    assert_eq!(apres["detected_manufacturer"], Value::Null, "{apres:#?}");
    assert_eq!(apres["detected_model"], Value::Null, "{apres:#?}");
    assert_eq!(
        apres["identite_appareil_effacee"],
        json!(true),
        "{apres:#?}"
    );
}

/// L'autre sens, et il est indispensable : le drapeau ne touche PAS
/// l'override. Sans ce témoin, couper toute identité resterait vert.
#[test]
fn le_vide_force_ne_mange_pas_l_override_de_l_utilisateur() {
    let (_, state) = serveur();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create(
            "Salon",
            Some("dlna"),
            Some("uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE"),
        )
        .unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(&format!("zone_{zone_id}_brand"), "WiiM")
        .unwrap();
    settings
        .set(&crate::routes::zones::cle_identite_effacee(zone_id), "true")
        .unwrap();

    let obj = identite(&state, zone_id, Some(&eversolo()));
    assert_eq!(
        obj["brand"],
        json!("WiiM"),
        "l'override reste roi : {obj:#?}"
    );
    assert_eq!(obj["detected_manufacturer"], Value::Null, "{obj:#?}");
}

/// L'APPELANT. Une valeur que la route n'écrit pas n'existe pas : ce témoin
/// passe par `PATCH /zones/{id}` et relit la base.
#[tokio::test]
async fn la_route_patch_ecrit_le_vide_force_et_sait_le_retirer() {
    let (app, state) = serveur();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create(
            "Salon",
            Some("dlna"),
            Some("uuid:9C41535E-DB73-11F0-A7C6-800A805D4DEE"),
        )
        .unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cle = crate::routes::zones::cle_identite_effacee(zone_id);

    let patch = |corps: Value| {
        let app = app.clone();
        async move {
            let requete = Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/zones/{zone_id}"))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap();
            app.oneshot(requete).await.unwrap().status()
        }
    };

    assert!(settings.get(&cle).unwrap().is_none(), "rien avant");
    let statut = patch(json!({"identite_appareil_effacee": true})).await;
    assert!(statut.is_success(), "PATCH refusé : {statut}");
    assert_eq!(
        settings.get(&cle).unwrap().as_deref(),
        Some("true"),
        "le champ est accepté par la route mais n'arrive nulle part"
    );
    assert!(
        crate::routes::zones::identite_appareil_effacee(&state.backend, zone_id),
        "la lecture partagée doit voir ce que la route a écrit"
    );

    let statut = patch(json!({"identite_appareil_effacee": false})).await;
    assert_eq!(statut, StatusCode::OK, "le retour en arrière doit passer");
    assert!(
        settings.get(&cle).unwrap().is_none(),
        "« je me suis trompé » doit rendre la détection"
    );
}
