//! #6257 (web#2166) — l'ordre des étiquettes, enregistré par `PUT /tags/order`
//! et suivi par toutes les routes qui listent des étiquettes.
//!
//! Tout passe par la ROUTE MONTÉE (`crate::routes::router`), sur une base
//! SQLite de FICHIER — jamais `:memory:` (#5043).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn etat(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, crate::state::AppState) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("etiquette-6257-{epreuve}"));
    let etat = crate::state::AppState::new(
        &dossier.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .unwrap();
    (dossier, etat)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn envoi(app: &axum::Router, methode: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    appel(app, req).await
}

async fn noms(app: &axum::Router, path: &str) -> Vec<String> {
    let (st, v) = appel(app, Request::get(path).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{path} : {v}");
    v.as_array()
        .unwrap_or_else(|| panic!("{path} ne rend pas une liste : {v}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn l_ordre_enregistre_est_rendu_par_toutes_les_listes() {
    let (_dossier, etat) = etat("routes");
    let app = crate::routes::router(etat.clone());

    let mut ids = Vec::new();
    for nom in ["Ambient", "Blues", "Cool jazz"] {
        let (st, v) = envoi(&app, "POST", "/api/v1/tags", json!({ "name": nom })).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        ids.push(v["id"].as_i64().unwrap());
    }
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    for id in [a, b, c] {
        let (st, v) = envoi(
            &app,
            "POST",
            &format!("/api/v1/tags/{id}/items"),
            json!({ "item_type": "album", "item_id": 7 }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
    }

    // Blues n'est pas nommée : elle garde son rang relatif, après les autres.
    let (st, v) = envoi(&app, "PUT", "/api/v1/tags/order", json!({ "ids": [c, a] })).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");

    let attendu = vec!["Cool jazz", "Ambient", "Blues"];
    for path in [
        "/api/v1/tags",
        "/api/v1/tags?item_type=album",
        "/api/v1/tags/search",
        "/api/v1/tags/search?q=",
        "/api/v1/tags/for/album/7",
    ] {
        assert_eq!(
            noms(&app, path).await,
            attendu,
            "{path} ne suit pas l'ordre enregistré"
        );
    }

    // `/order` n'est pas pris pour un identifiant d'étiquette.
    let (st, _) = envoi(
        &app,
        "PUT",
        &format!("/api/v1/tags/{b}"),
        json!({ "name": "Blues" }),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

/// Décision de Bertrand (13/10) : tant que rien n'a été réordonné, la liste
/// reste alphabétique et une étiquette neuve s'y range par son nom ; après un
/// `PUT /tags/order`, une neuve va en fin de liste.
#[tokio::test]
async fn une_neuve_se_range_par_nom_tant_qu_aucun_ordre_n_est_choisi() {
    let (_dossier, etat) = etat("neuve");
    let app = crate::routes::router(etat.clone());

    let mut ids = Vec::new();
    for nom in ["Rock", "Blues", "Jazz", "Ambient"] {
        let (st, v) = envoi(&app, "POST", "/api/v1/tags", json!({ "name": nom })).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        ids.push(v["id"].as_i64().unwrap());
    }
    assert_eq!(
        noms(&app, "/api/v1/tags").await,
        vec!["Ambient", "Blues", "Jazz", "Rock"],
        "sans ordre choisi, la liste doit rester alphabétique"
    );

    let (st, v) = envoi(
        &app,
        "PUT",
        "/api/v1/tags/order",
        json!({ "ids": [ids[0], ids[1], ids[2], ids[3]] }),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    let (st, v) = envoi(&app, "POST", "/api/v1/tags", json!({ "name": "Afrobeat" })).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(
        noms(&app, "/api/v1/tags").await,
        vec!["Rock", "Blues", "Jazz", "Ambient", "Afrobeat"],
        "après un ordre choisi, une neuve doit aller en fin de liste"
    );
}
