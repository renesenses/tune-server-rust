//! #7195 — le jeton Discogs des « Jetons de service » est le jeton PERSONNEL.
//!
//! Essai réel de Bertrand sur le .18, le 31/10/2026 : jeton saisi dans
//! Réglages ▸ « Jetons de service » (« Token valide (utilisateur: …) »),
//! mais « Ma collection physique » disait « sans jeton personnel » et
//! `GET /system/api-keys` rendait `personnelle: false, utilisee: "integree"`.
//! Deux réserves pour le même jeton : il n'en reste qu'une, le coffre des
//! Clés d'API, lue par `cles_api::resoudre`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_collection_physique::{Hote, Jeton};
use tune_core::services_manager::{ServicesManager, TokenPayload};

use super::HoteDeLaCollection;
use crate::state::AppState;

const JETON: &str = "JETON-de-service-discogs-7195-a1b2c3d4";

fn etat(dir: &std::path::Path) -> AppState {
    let config = crate::config::TuneConfig {
        db_path: dir.join("tune.db").to_string_lossy().to_string(),
        ..Default::default()
    };
    AppState::new(":memory:", 0, config).expect("état en mémoire")
}

fn hote(e: &AppState) -> HoteDeLaCollection {
    HoteDeLaCollection { state: e.clone() }
}

async fn appel(e: &AppState, methode: &str, chemin: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .body(Body::empty())
        .unwrap();
    let r = crate::routes::system::router()
        .with_state(e.clone())
        .oneshot(req)
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).to_string())
}

fn discogs_dans(corps: &str) -> Value {
    let v: Value = serde_json::from_str(corps).unwrap();
    v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["service"] == "discogs")
        .cloned()
        .unwrap()
}

/// La voie « Jetons de service » (`POST /services/tokens/discogs`, sa part
/// qui range avant la validation par Discogs) : le greffon voit `Personnel`,
/// l'écran des Clés d'API aussi, et le jeton n'apparaît dans aucune réponse.
#[tokio::test]
async fn le_jeton_des_jetons_de_service_est_vu_personnel_par_le_greffon() {
    let dir = tempfile::tempdir().unwrap();
    let e = etat(dir.path());
    assert_eq!(hote(&e).jeton_discogs(), Jeton::Aucun);

    crate::routes::service_tokens::ranger_les_champs(&e, "discogs", &json!({"token": JETON}))
        .unwrap();

    assert_eq!(hote(&e).jeton_discogs(), Jeton::Personnel);
    let (s, corps) = appel(&e, "GET", "/api-keys").await;
    assert_eq!(s, StatusCode::OK);
    assert!(!corps.contains(JETON), "le jeton ne sort jamais : {corps}");
    let d = discogs_dans(&corps);
    assert_eq!(d["personnelle"], true, "{d}");
    assert_eq!(d["utilisee"], "personnelle", "{d}");
    // L'enrichissement lit la même valeur qu'avant.
    assert_eq!(
        crate::routes::discogs::discogs_token(&e).as_deref(),
        Some(JETON)
    );
}

/// Une installation existante (jeton saisi dans « Jetons de service » avant
/// l'unification : ligne `streaming_auth` et réglage `discogs_token`) l'adopte
/// au démarrage, sans ressaisie ; une seconde fois ne change rien.
#[tokio::test]
async fn une_installation_existante_adopte_son_jeton_au_demarrage() {
    let dir = tempfile::tempdir().unwrap();
    let e = etat(dir.path());
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(e.backend.clone());
    settings.set("discogs_token", JETON).unwrap();
    let mut fields = std::collections::HashMap::new();
    fields.insert("token".to_string(), json!(JETON));
    ServicesManager::with_backend(e.backend.clone())
        .save_token(
            "discogs",
            &TokenPayload {
                fields,
                valid: Some(true),
                validation_message: Some("Token valide (utilisateur: x).".into()),
                validated_at: Some(1),
            },
        )
        .unwrap();
    // Avant : vu comme la clé du serveur.
    assert_eq!(hote(&e).jeton_discogs(), Jeton::Integre);

    assert!(crate::routes::system::cles_api::adopter_le_jeton_discogs_des_jetons_de_service(&e));
    assert_eq!(hote(&e).jeton_discogs(), Jeton::Personnel);
    assert!(!crate::routes::system::cles_api::adopter_le_jeton_discogs_des_jetons_de_service(&e));

    // Retirer le jeton personnel (Clés d'API) le retire des deux écrans : le
    // démarrage suivant ne le réadopte pas.
    let (s, corps) = appel(&e, "DELETE", "/api-keys/discogs").await;
    assert_eq!(s, StatusCode::OK, "{corps}");
    assert!(!corps.contains(JETON));
    assert_eq!(hote(&e).jeton_discogs(), Jeton::Aucun);
    assert!(!crate::routes::system::cles_api::adopter_le_jeton_discogs_des_jetons_de_service(&e));
}

/// Le jeton du SERVEUR (`tune.toml`, environnement) reste « intégré » : seul
/// ce que l'utilisateur a saisi devient personnel.
#[tokio::test]
async fn le_jeton_du_serveur_reste_integre() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::TuneConfig {
        db_path: dir.path().join("tune.db").to_string_lossy().to_string(),
        discogs_token: Some("JETON-DU-SERVEUR-0000".into()),
        ..Default::default()
    };
    let e = AppState::new(":memory:", 0, config).unwrap();
    assert!(!crate::routes::system::cles_api::adopter_le_jeton_discogs_des_jetons_de_service(&e));
    assert_eq!(hote(&e).jeton_discogs(), Jeton::Integre);
}
