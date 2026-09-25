//! #4863 — le greffon `cd` au catalogue, de bout en bout, à travers trois
//! démarrages sur la MÊME base : proposé, installé par la route existante,
//! actif (ses routes répondent), désinstallé, de nouveau seulement proposé.
//!
//! Le démarrage est rejoué pour de vrai (un `AppState` neuf sur le même
//! fichier SQLite) parce que la porte d'installation ne s'ouvre qu'au
//! démarrage : un seul état aurait prouvé l'écriture du réglage, pas l'effet.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

fn demarrer(base: &std::path::Path) -> AppState {
    AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
}

async fn appel(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let req = Request::builder().method(methode).uri(chemin);
    let req = if methode == "POST" {
        req.header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap()
    } else {
        req.body(Body::empty()).unwrap()
    };
    let rep = app.clone().oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

/// La fiche `cd` de `/api/v1/plugins`, ou `None` si le gestionnaire ne la
/// montre pas.
async fn fiche_cd(app: &axum::Router) -> Option<Value> {
    let (code, liste) = appel(app, "GET", "/api/v1/plugins").await;
    assert_eq!(code, StatusCode::OK);
    liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .find(|p| p["name"] == "cd")
        .cloned()
}

#[tokio::test]
async fn cd_est_propose_s_installe_repond_et_se_desinstalle() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : jamais installé → proposé au catalogue.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "cd"),
        "opt-in : rien ne tourne avant l'installation"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_cd(&app)
        .await
        .expect("« cd » doit être PROPOSÉ par le gestionnaire (catalogue, #4863)");
    assert_eq!(fiche["type"], "sdk", "{fiche}");
    assert_eq!(fiche["installed"], false, "{fiche}");
    assert_eq!(fiche["enabled"], false, "{fiche}");
    assert_eq!(
        fiche["compatible"], true,
        "bouton « Installer » actif — {fiche}"
    );
    assert_eq!(fiche["premium"], false, "gratuit, comme bandcamp — {fiche}");
    assert_eq!(fiche["url"], "/api/v1/ext/cd", "{fiche}");
    assert!(
        fiche["description"]
            .as_str()
            .is_some_and(|d| d.contains("Linux")),
        "la fiche doit dire la plateforme prise en charge — {fiche}"
    );
    let (code, _) = appel(&app, "GET", "/api/v1/ext/cd/etat").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "non installé : routes non montées"
    );

    // Installation par la route existante.
    let (code, rep) = appel(&app, "POST", "/api/v1/plugins/cd/install").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 2e démarrage : installé → actif, et ses routes répondent.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        routeurs.iter().any(|(n, _)| n == "cd"),
        "installé : le greffon doit monter son routeur (montés : {:?})",
        routeurs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_cd(&app)
        .await
        .expect("un greffon qui tourne est listé");
    assert_eq!(fiche["installed"], true, "{fiche}");
    assert_eq!(fiche["enabled"], true, "{fiche}");
    let (code, lecteur) = appel(&app, "GET", "/api/v1/ext/cd/etat").await;
    assert_eq!(code, StatusCode::OK, "{lecteur}");
    assert_eq!(
        lecteur["plateforme_prise_en_charge"],
        cfg!(target_os = "linux"),
        "hors Linux, un ÉTAT « non pris en charge », pas une erreur — {lecteur}"
    );
    assert!(lecteur["presence"].is_string(), "{lecteur}");

    // Désinstallation.
    let (code, rep) = appel(&app, "DELETE", "/api/v1/plugins/cd").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 3e démarrage : retiré → plus rien ne tourne, de nouveau proposé.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(!routeurs.iter().any(|(n, _)| n == "cd"), "désinstallé");
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_cd(&app).await.expect("toujours proposé");
    assert_eq!(fiche["installed"], false, "{fiche}");
    let (code, _) = appel(&app, "GET", "/api/v1/ext/cd/etat").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "désinstallé : routes démontées"
    );
}
