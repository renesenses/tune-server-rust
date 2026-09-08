//! #3462 — un SlimProto mort au démarrage doit se voir depuis un écran.
//!
//! Belkadi Yacine fait tourner un Lyrion/LMS sur la même machine que Tune. Le
//! bind de SlimProto sur 3483 échoue, **aucune platine Squeezebox ne verra
//! jamais Tune**, et le serveur continue comme si de rien n'était : la panne ne
//! vivait que dans une ligne de journal d'une tâche détachée, plus dans un état
//! de session que seuls `/system/diagnostics/network` et le rapport de bogue
//! servaient — deux chemins qu'on n'emprunte qu'après avoir déjà soupçonné
//! quelque chose. Le testeur, lui, regarde la grille de composants de l'écran
//! Diagnostics et de l'onglet Système.
//!
//! Ce fichier vit à part parce que `tune_core::slimproto::etat_ecoute()` est un
//! état **global au processus** : le poser depuis un test contaminerait tous
//! les autres tests du même binaire. Une cible de test = un processus.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : sans l'entrée
//! `[[test]]` correspondante, ce fichier ne serait JAMAIS compilé et cette
//! garde serait verte contre rien.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn sante(app: &axum::Router) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/system/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Le parcours complet, dans l'ordre : rien à dire, puis un port tenu par un
/// autre serveur, puis ce que l'écran reçoit.
///
/// Les deux moitiés sont dans la MÊME épreuve parce que l'état est global :
/// séparées en deux tests, l'ordre d'exécution déciderait du résultat.
#[tokio::test]
async fn un_slimproto_hors_service_apparait_dans_les_composants_de_sante() {
    // ---- 1. Témoin d'origine : aucune tentative d'écoute, aucun composant.
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    let (code, corps) = sante(&app).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        corps["components"]["slimproto"].is_null(),
        "tant qu'aucune écoute n'a été tentée, l'absence reste une absence — \
         elle ne devient pas « en panne » : {corps}"
    );
    assert_eq!(corps["status"], "ok");

    // ---- 2. Un autre serveur tient le port : c'est la situation du testeur.
    let squatteur = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = squatteur.local_addr().unwrap().port();

    let serveur = Arc::new(tune_core::slimproto::SlimProtoServer::new_sur_port(port));
    serveur
        .spawn()
        .await
        .expect_err("le bind devait échouer : le port est déjà tenu");

    // ---- 3. Ce que l'écran reçoit.
    let (code, corps) = sante(&app).await;
    assert_eq!(
        corps["components"]["slimproto"],
        Value::Bool(false),
        "le sous-système est mort et l'écran doit pouvoir le dire : {corps}"
    );

    // ⚠️ La moitié qui compte autant que l'autre. `status` et le 503 énoncent
    // l'état de la BASE ; un LMS voisin n'a jamais empêché Tune de servir sa
    // bibliothèque. Faire basculer tout le serveur en « degraded » pour ça
    // allumerait la pastille orange de la barre latérale chez tous ceux qui
    // font tourner un Lyrion — un faux rouge permanent.
    assert_eq!(
        corps["status"], "ok",
        "un SlimProto hors service ne dégrade pas le verdict global : {corps}"
    );
    assert_eq!(code, StatusCode::OK);
    assert_eq!(corps["db"], "connected");
    assert_eq!(
        corps["components"]["db_tracks"],
        Value::Bool(true),
        "les sondes d'origine ne bougent pas : {corps}"
    );

    drop(squatteur);
}
