//! #5296 — le greffon `entree-audio` AU CATALOGUE des extensions, de bout en
//! bout, à travers trois démarrages sur la MÊME base, comme
//! `cd_catalogue_4863` : proposé, installé par la route existante, actif (ses
//! routes répondent, la source PCM est inscrite), désinstallé, de nouveau
//! seulement proposé.
//!
//! Avant #5296, il était compilé dans `default` (#5051) mais HORS catalogue :
//! il fallait un `curl -X POST …/plugins/entree-audio/install` à la main pour
//! voir les entrées USB et virtuelles (Loopback, BlackHole) dans la rubrique
//! Sources (#5065). Constaté sur le Mac de Bertrand avec Loopback Audio.
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

/// La fiche `entree-audio` de `/api/v1/plugins`, ou `None` si le
/// gestionnaire ne la montre pas.
async fn fiche(app: &axum::Router) -> Option<Value> {
    let (code, liste) = appel(app, "GET", "/api/v1/plugins").await;
    assert_eq!(code, StatusCode::OK);
    liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .find(|p| p["name"] == "entree-audio")
        .cloned()
}

#[tokio::test]
async fn entree_audio_est_propose_s_installe_repond_et_se_desinstalle() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : jamais installé → PROPOSÉ au catalogue, rien ne tourne.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "entree-audio"),
        "opt-in : rien ne tourne avant l'installation"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let carte = fiche(&app).await.expect(
        "#5296 : « entree-audio » doit être PROPOSÉ par le gestionnaire des \
         extensions (catalogue), pas installable seulement à la main",
    );
    assert_eq!(carte["type"], "sdk", "{carte}");
    assert_eq!(carte["installed"], false, "{carte}");
    assert_eq!(carte["enabled"], false, "{carte}");
    assert_eq!(
        carte["compatible"], true,
        "bouton « Installer » actif — {carte}"
    );
    assert_eq!(carte["premium"], false, "gratuit, comme cd — {carte}");
    assert_eq!(carte["url"], "/api/v1/ext/entree-audio", "{carte}");
    assert_eq!(
        carte["display_name"], "Entrée audio",
        "la carte proposée porte un nom lisible, pas l'identifiant — {carte}"
    );
    let (code, _) = appel(&app, "GET", "/api/v1/ext/entree-audio/etat").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "non installé : routes non montées"
    );

    // Installation par la route existante, celle du bouton « Installer ».
    let (code, rep) = appel(&app, "POST", "/api/v1/plugins/entree-audio/install").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 2e démarrage : installé → actif, ses routes répondent.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        routeurs.iter().any(|(n, _)| n == "entree-audio"),
        "installé : le greffon doit monter son routeur (montés : {:?})",
        routeurs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let carte = fiche(&app).await.expect("un greffon qui tourne est listé");
    assert_eq!(carte["installed"], true, "{carte}");
    assert_eq!(carte["enabled"], true, "{carte}");
    assert_eq!(
        carte["display_name"], "Entrée audio",
        "la carte d'un greffon qui tourne garde son nom lisible — {carte}"
    );
    let (code, v) = appel(&app, "GET", "/api/v1/ext/entree-audio/etat").await;
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

    // Désinstallation.
    let (code, rep) = appel(&app, "DELETE", "/api/v1/plugins/entree-audio").await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["restart_required"], true, "{rep}");
    drop(app);
    drop(etat);

    // ── 3e démarrage : retiré → plus rien ne tourne, de nouveau proposé.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "entree-audio"),
        "désinstallé"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let carte = fiche(&app).await.expect("toujours proposé");
    assert_eq!(carte["installed"], false, "{carte}");
    let (code, _) = appel(&app, "GET", "/api/v1/ext/entree-audio/etat").await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "désinstallé : routes démontées"
    );
}
