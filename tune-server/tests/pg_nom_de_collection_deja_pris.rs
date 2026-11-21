//! Forum, 21/11/2026 — un nom de collection intelligente déjà pris, sur une
//! VRAIE base PostgreSQL, par le vrai routeur.
//!
//! Le schéma PostgreSQL n'a PAS l'index unique `idx_smart_collections_name`
//! de SQLite (migration 49) : avant ce correctif, le doublon y passait sans
//! bruit (deux collections du même nom). La vérification préalable des
//! routes le refuse désormais en 409 `smart_collection_name_taken`, comme sur
//! SQLite (`tune-smart-http/src/nom_deja_pris_tests.rs`).
//!
//! Même doctrine que `pg_requete_avancee_7872.rs` : variable ABSENTE ⇒ saut
//! annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes.
const MARQUE: &str = "nompris2111";

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &AppState) {
    let sql = format!("DELETE FROM smart_collections WHERE name LIKE '{MARQUE}%'");
    state
        .backend
        .execute(&sql, &[])
        .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
}

async fn appel(state: &AppState, methode: &str, route: &str, corps: Value) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let requete = Request::builder()
        .method(methode)
        .uri(route)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    let json = serde_json::from_str(&texte)
        .unwrap_or_else(|e| panic!("{route} → {statut} : JSON illisible ({e}) : {texte}"));
    (statut, json)
}

fn combien_s_appellent(state: &AppState, nom: &str) -> i64 {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM smart_collections WHERE name = ?",
            &[&nom as &dyn ToSqlValue],
        )
        .expect("compte")
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_nom_de_collection_deja_pris_rend_409() {
    let Some(url) = std::env::var("TUNE_TEST_PG_URL").ok() else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);
    let pris = format!("{MARQUE} Jazz vocal des années cinquante");
    let autre = format!("{MARQUE} Piano solo");
    let route = "/api/v1/library/smart-collections";

    let (statut, a) = appel(&state, "POST", route, json!({"name": pris, "rules": []})).await;
    assert_eq!(statut, StatusCode::CREATED, "{a}");
    let id_a = a["id"].as_i64().expect("id");

    // Création en double : 409, et toujours UNE seule collection de ce nom.
    let (statut, v) = appel(&state, "POST", route, json!({"name": pris, "rules": []})).await;
    assert_eq!(statut, StatusCode::CONFLICT, "doublon : {v}");
    assert_eq!(v["code"], "smart_collection_name_taken", "{v}");
    assert_eq!(combien_s_appellent(&state, &pris), 1);

    // Renommage vers un nom pris : 409, le nom d'origine est intact.
    let (statut, b) = appel(&state, "POST", route, json!({"name": autre, "rules": []})).await;
    assert_eq!(statut, StatusCode::CREATED, "{b}");
    let id_b = b["id"].as_i64().expect("id");
    let (statut, v) = appel(
        &state,
        "PUT",
        &format!("{route}/{id_b}"),
        json!({"name": pris}),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "renommage : {v}");
    assert_eq!(v["code"], "smart_collection_name_taken", "{v}");
    assert_eq!(combien_s_appellent(&state, &autre), 1);
    assert_eq!(combien_s_appellent(&state, &pris), 1);

    // Renommage vers son propre nom : passe.
    let (statut, v) = appel(
        &state,
        "PUT",
        &format!("{route}/{id_a}"),
        json!({"name": pris, "description": "même nom"}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["name"], pris.as_str());

    menage(&state);
}
