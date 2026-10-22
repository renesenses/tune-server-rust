//! Chantier « Assistant IA », étape 2 — l'ancien assistant `POST /ai/query`
//! ne lit plus la clé EN CLAIR `anthropic_api_key` : elle est migrée dans le
//! coffre partagé du serveur, et le plafond mensuel de l'assistant borne ses appels.
//!
//! Par la ROUTE MONTÉE, sans aucun appel réseau : le plafond est posé à zéro,
//! la requête est donc refusée AVANT de partir. Si la route lisait encore la
//! clé en clair (chemin historique), elle tenterait d'appeler Anthropic et
//! rendrait une erreur 500 ; la clé en clair resterait dans `settings`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` de `tune-server/Cargo.toml`. Il pose
//! `TUNE_DB_PATH` (le dossier de la clé maîtresse) : un seul essai, dans sa
//! propre cible, pour ne gêner personne.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "sk-ant-api03-ANCIENNE-EN-CLAIR-0000-abcd";

#[tokio::test]
async fn l_ancienne_cle_en_clair_est_migree_et_le_plafond_borne_l_ancien_assistant() {
    let dossier = tempfile::tempdir().expect("dossier");
    // SAFETY : seul essai de cette cible, posé avant toute lecture.
    unsafe { std::env::set_var("TUNE_DB_PATH", dossier.path().join("tune.db")) };
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("anthropic_api_key", CLE).expect("clé héritée");
    settings
        .set("assistant_ia_plafond_eur", "0")
        .expect("plafond");

    let reponse = tune_server::routes::router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ai/query")
                .header("content-type", "application/json")
                .body(Body::from(json!({"message": "joue du jazz"}).to_string()))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .expect("corps");
    let v: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);

    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(
        v["error"], "plafond_atteint",
        "l'ancien assistant doit passer par le plafond de l'assistant ; reçu : {v}"
    );
    assert!(!v.to_string().contains(CLE), "la clé ne sort jamais : {v}");
    assert_eq!(
        settings.get("anthropic_api_key").expect("lecture"),
        None,
        "la clé en clair doit être effacée après migration"
    );
    assert!(
        settings
            .get("assistant_ia_api_key_chiffree")
            .expect("lecture")
            .is_some_and(|s| !s.contains("ANCIENNE")),
        "la clé doit être scellée"
    );
    // Scellée sous la clé maîtresse PARTAGÉE du serveur (un seul coffre,
    // décision du 12/10/2026) : l'ancien fichier du greffon ne naît plus.
    assert!(dossier.path().join(tune_core::coffre::FICHIER_CLE).exists());
    assert!(!dossier.path().join("assistant-ia.cle").exists());
}
