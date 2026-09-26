//! #5051 — le greffon `entree-audio` : compilé dans `default`, HORS
//! catalogue (aucun écran ne consomme encore ses routes, doctrine #2090),
//! dormant tant qu'il n'est pas installé, et ses routes répondent une fois
//! installé. Deux démarrages sur la MÊME base, comme `cd_catalogue_4863`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

fn demarrer(base: &std::path::Path) -> AppState {
    AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
}

async fn appel(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(chemin)
        .body(Body::empty())
        .unwrap();
    let rep = app.clone().oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

#[tokio::test]
async fn entree_audio_est_hors_catalogue_dormant_puis_repond_une_fois_installe() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : ni proposé, ni monté.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "entree-audio"),
        "opt-in : rien ne tourne avant l'installation"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let (code, liste) = appel(&app, "/api/v1/plugins").await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        !liste
            .as_array()
            .expect("tableau")
            .iter()
            .any(|p| p["name"] == "entree-audio"),
        "hors catalogue : le gestionnaire ne le propose pas — {liste}"
    );
    let (code, _) = appel(&app, "/api/v1/ext/entree-audio/etat").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "non installé : routes non montées"
    );
    // Installé à la main (hors catalogue : pas de bouton « Installer »).
    tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone())
        .set("plugin_entree-audio_installed", "true")
        .unwrap();
    drop(app);
    drop(etat);

    // ── 2e démarrage : installé → ses routes répondent.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        routeurs.iter().any(|(n, _)| n == "entree-audio"),
        "installé : le greffon doit monter son routeur"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let (code, v) = appel(&app, "/api/v1/ext/entree-audio/etat").await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["active"], false, "{v}");
    assert_eq!(
        v["capture_compilee"],
        cfg!(feature = "local-audio"),
        "la capture suit la pile audio locale — {v}"
    );
    assert!(v["autorisation"].is_string(), "{v}");
    assert!(
        etat.orchestrator
            .sources_pcm()
            .fournisseur("entree-audio")
            .is_some_and(|f| f.en_direct()),
        "la source `entree-audio` est inscrite EN DIRECT auprès de l'orchestrateur"
    );
}
