//! #5065 — `GET /api/v1/sources` et `POST /api/v1/sources/{id}/jouer` à
//! travers le VRAI serveur : routes montées, registre porté par
//! l'orchestrateur, greffon `cd` qui s'y inscrit quand il est installé et
//! disparaît quand il est désinstallé (trois démarrages sur la même base,
//! comme `cd_catalogue_4863.rs` : la porte d'installation ne s'ouvre qu'au
//! démarrage).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json");
    let req = if methode == "GET" {
        req.body(Body::empty()).unwrap()
    } else {
        req.body(Body::from(corps.to_string())).unwrap()
    };
    let rep = app.clone().oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

async fn demarrer(base: &std::path::Path) -> axum::Router {
    let etat = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    tune_server::routes::router_with_plugins(etat, routeurs)
}

async fn sources(app: &axum::Router) -> Value {
    let (code, v) = appel(app, "GET", "/api/v1/sources", Value::Null).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert!(v.is_array(), "{v}");
    v
}

#[tokio::test]
async fn sources_suit_l_installation_du_greffon_cd() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── Non installé : aucune source, et une source inconnue rend 404.
    let app = demarrer(&base).await;
    assert_eq!(sources(&app).await, json!([]));
    let (code, v) = appel(
        &app,
        "POST",
        "/api/v1/sources/cd/jouer",
        json!({ "zone_id": 1 }),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"], "source_inconnue", "{v}");
    let (code, _) = appel(&app, "POST", "/api/v1/plugins/cd/install", json!({})).await;
    assert_eq!(code, StatusCode::OK);
    drop(app);

    // ── Installé : la source `cd` suit la machine. Hors plateforme prise en
    // charge, elle est là en `non_pris_en_charge` et « jouer » rend 409 ;
    // sur une plateforme prise en charge SANS lecteur (Shrek), rien.
    let app = demarrer(&base).await;
    let liste = sources(&app).await;
    if !tune_cd::lecteur::plateforme_prise_en_charge() {
        assert_eq!(
            liste,
            json!([{ "id": "cd", "type": "cd", "greffon": "cd", "nom": "Lecteur CD",
                     "etat": "non_pris_en_charge", "detail": {} }])
        );
        let (code, v) = appel(
            &app,
            "POST",
            "/api/v1/sources/cd/jouer",
            json!({ "zone_id": 1 }),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "lecture_non_prise_en_charge", "{v}");
    } else if tune_cd::lecteur::lecteur_du_systeme().is_none() {
        assert_eq!(liste, json!([]));
    }
    let (code, _) = appel(&app, "DELETE", "/api/v1/plugins/cd", Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    drop(app);

    // ── Désinstallé : absent.
    let app = demarrer(&base).await;
    assert_eq!(sources(&app).await, json!([]));
}
