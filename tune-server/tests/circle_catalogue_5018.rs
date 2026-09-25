//! #5018 — le greffon `circle` (Tune Circle) au catalogue, de bout en bout, à
//! travers trois démarrages sur la MÊME base : proposé, installé par la route
//! existante, actif (sa route répond), désinstallé, de nouveau seulement
//! proposé.
//!
//! Le démarrage est rejoué pour de vrai (un `AppState` neuf sur le même
//! fichier SQLite) parce que la porte d'installation ne s'ouvre qu'au
//! démarrage : un seul état aurait prouvé l'écriture du réglage, pas l'effet.
//!
//! Aucun réseau : sans session SSO, `GET /ext/circle/` répond sans appeler le
//! cloud.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
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

/// La fiche `circle` de `/api/v1/plugins`, ou `None` si le gestionnaire ne la
/// montre pas.
async fn fiche_circle(app: &axum::Router) -> Option<Value> {
    let (code, liste) = appel(app, "GET", "/api/v1/plugins").await;
    assert_eq!(code, StatusCode::OK);
    liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .find(|p| p["name"] == "circle")
        .cloned()
}

#[tokio::test]
async fn circle_est_propose_s_installe_repond_et_se_desinstalle() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : jamais installé → proposé au catalogue.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "circle"),
        "opt-in : rien ne tourne avant l'installation"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_circle(&app)
        .await
        .expect("« circle » doit être PROPOSÉ par le gestionnaire (catalogue, #5018)");
    assert_eq!(fiche["type"], "sdk", "{fiche}");
    assert_eq!(fiche["installed"], false, "{fiche}");
    assert_eq!(fiche["enabled"], false, "{fiche}");
    assert_eq!(
        fiche["compatible"], true,
        "bouton « Installer » actif — {fiche}"
    );
    assert_eq!(
        fiche["premium"], false,
        "gratuit (décision du 25/09) — {fiche}"
    );
    assert_eq!(fiche["url"], "/api/v1/ext/circle", "{fiche}");
    // Sans barre finale : l'hôte redirige `…/` (308) vers la forme sans barre.
    let (code, _) = appel(&app, "GET", "/api/v1/ext/circle").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "non installé : routes non montées"
    );

    // Installation par la route existante.
    let (code, rep) = appel(&app, "POST", "/api/v1/plugins/circle/install").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 2e démarrage : installé → actif, et sa route répond.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        routeurs.iter().any(|(n, _)| n == "circle"),
        "installé : le greffon doit monter son routeur (montés : {:?})",
        routeurs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_circle(&app)
        .await
        .expect("un greffon qui tourne est listé");
    assert_eq!(fiche["installed"], true, "{fiche}");
    assert_eq!(fiche["enabled"], true, "{fiche}");
    let (code, cercle) = appel(&app, "GET", "/api/v1/ext/circle/").await;
    assert_eq!(code, StatusCode::OK, "{cercle}");
    assert_eq!(
        cercle,
        json!({ "connected": false }),
        "sans session SSO : un état, pas une erreur"
    );

    // Désinstallation.
    let (code, rep) = appel(&app, "DELETE", "/api/v1/plugins/circle").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 3e démarrage : retiré → plus rien ne tourne, de nouveau proposé.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(!routeurs.iter().any(|(n, _)| n == "circle"), "désinstallé");
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let fiche = fiche_circle(&app).await.expect("toujours proposé");
    assert_eq!(fiche["installed"], false, "{fiche}");
    // Sans barre finale : l'hôte redirige `…/` (308) vers la forme sans barre.
    let (code, _) = appel(&app, "GET", "/api/v1/ext/circle").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "désinstallé : routes démontées"
    );
}
