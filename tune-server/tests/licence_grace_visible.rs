//! #1999 — la grâce hors ligne de 14 jours doit être VISIBLE par l'API.
//!
//! Tune accorde 14 jours de tolérance quand la validation en ligne ne peut pas
//! aboutir (`tune-core/src/license.rs`, `GRACE_PERIOD_DAYS`). C'est une bonne
//! chose, mais jusqu'ici elle ne se manifestait que par un `warn!` dans le
//! journal, le quatorzième jour, une fois le premium déjà perdu. Didier (fil
//! forum 1491) posait la question avant d'acheter : rien ne pouvait lui
//! répondre à l'écran.
//!
//! Ces tests verrouillent le contrat HTTP que lit le client web. Ils sont
//! **hermétiques** : aucun appel au serveur de licences, tout l'état est écrit
//! directement dans une base en mémoire, exactement comme après un démarrage.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn past_iso(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Base de travail jetable, à un chemin unique par test — jamais un nom fixe
/// dans `temp_dir()`, deux tests en parallèle se marcheraient dessus.
///
/// Le garde est rendu avec le chemin : tant qu'il vit, la base vit ; quand il
/// sort de portée — panique comprise — le dossier disparaît. La composition à
/// la main laissait un `tune-i1999-*` par exécution (#3030).
fn scratch_db() -> (tune_core::test_scratch::ScratchDir, std::path::PathBuf) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i1999");
    let db = dir.join("library.db");
    (dir, db)
}

/// Écrit l'état d'une machine premium dont la dernière validation en ligne
/// remonte à `days` jours, puis rend un routeur qui le relit **au démarrage**.
/// Rien ne sort sur le réseau : c'est exactement le chemin du démarrage hors
/// ligne, celui que personne n'avait relu.
fn app_premium_validated_days_ago(
    days: i64,
) -> (axum::Router, tune_core::test_scratch::ScratchDir) {
    let (base, db_path) = scratch_db();
    let db_path = db_path.to_str().unwrap().to_string();

    {
        let state = AppState::new(&db_path, 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings.set("license_key", "TUNE-TEST-0000-9999").unwrap();
        settings.set("license_tier", "premium").unwrap();
        settings
            .set("license_last_validated", &past_iso(days))
            .unwrap();
    }

    // Le LicenseManager lit les settings à sa construction : rouvrir la MÊME
    // base rejoue le démarrage d'une machine restée hors ligne.
    let state = AppState::new(&db_path, 0, Default::default()).unwrap();
    (tune_server::routes::router(state), base)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

const STATUS: &str = "/api/v1/cloud/license/status";

#[tokio::test]
async fn le_statut_porte_toujours_le_champ_de_grace() {
    // Même sur une installation gratuite, le champ existe (à `null`) : le client
    // n'a pas à deviner si le serveur en face sait répondre.
    let app = tune_server::routes::router(new_state());
    let (status, body) = get_json(&app, STATUS).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("offline_grace").is_some(),
        "champ absent de la réponse : {body}"
    );
    assert!(
        body["offline_grace"].is_null(),
        "rien à annoncer sans droits premium : {body}"
    );
}

#[tokio::test]
async fn une_machine_hors_ligne_depuis_trois_jours_le_dit_et_reste_premium() {
    let (app, _base) = app_premium_validated_days_ago(3);
    let (_, body) = get_json(&app, STATUS).await;
    let g = &body["offline_grace"];

    assert_eq!(g["phase"], "grace", "état annoncé : {body}");
    assert_eq!(g["source"], "key");
    assert_eq!(g["total_days"], 14, "le chiffre affiché vient du code");
    assert_eq!(g["days_remaining"], 11);
    assert_eq!(g["days_since_validation"], 3);
    assert!(g["since"].is_string(), "depuis quand : {g}");
    assert!(g["until"].is_string(), "jusqu'à quand : {g}");

    // Et surtout : rien n'est durci. Le serveur est toujours premium.
    assert_eq!(body["tier"], "premium", "la grâce visible ne dégrade rien");
}

#[tokio::test]
async fn le_dernier_jour_annonce_encore_un_jour_entier() {
    // Un arrondi vers le bas afficherait « 0 jour » à un utilisateur encore
    // parfaitement premium.
    let (app, _base) = app_premium_validated_days_ago(13);
    let (_, body) = get_json(&app, STATUS).await;
    assert_eq!(body["offline_grace"]["days_remaining"], 1);
    assert_eq!(body["offline_grace"]["phase"], "grace");
    assert_eq!(body["tier"], "premium");
}

#[tokio::test]
async fn au_dela_de_quatorze_jours_la_reponse_explique_la_retombee() {
    let (app, _base) = app_premium_validated_days_ago(20);
    let (_, body) = get_json(&app, STATUS).await;
    assert_eq!(body["offline_grace"]["phase"], "expired");
    assert_eq!(body["offline_grace"]["days_remaining"], 0);
    // Comportement inchangé : c'est exactement ce que faisait le serveur avant
    // #1999, il ne le disait simplement pas.
    assert_eq!(body["tier"], "free");
}

#[tokio::test]
async fn la_reponse_de_grace_ne_contient_aucune_donnee_de_licence() {
    // La clé est un secret d'authentification (elle ouvre le support premium
    // côté mozaiklabs). Le bloc de grâce ne doit porter que des dates et des
    // compteurs.
    let (app, _base) = app_premium_validated_days_ago(3);
    let (_, body) = get_json(&app, STATUS).await;
    let brut = body["offline_grace"].to_string();
    assert!(
        !brut.contains("TUNE-TEST"),
        "clé fuitée dans le bloc de grâce : {brut}"
    );
    assert!(
        !brut.contains("fingerprint"),
        "empreinte machine fuitée : {brut}"
    );
}
