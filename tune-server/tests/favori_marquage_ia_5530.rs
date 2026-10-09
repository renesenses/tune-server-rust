//! #5530 (FabienM, fil 2053) — le marquage « généré par IA » envoyé avec un
//! favori de service est rangé, et la liste des favoris le rend.
//!
//! La route `POST /profiles/{id}/favorites/streaming/add` accepte
//! `ai_generated` (le client l'a quand Qobuz l'a rendu sur l'album). Absent :
//! rien n'est posé, et la liste ne porte pas la clé.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", "1");
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

#[tokio::test]
async fn le_favori_garde_le_marquage_ia_envoye_par_le_client() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    let (s, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/streaming/add",
        Some(json!({
            "item_type": "album", "service": "qobuz", "service_id": "tj9je5zd70wsc",
            "title": "Psychedelic Mongolian Trip Hop (\"Painted Yurts, Painted Souls\") AI Album",
            "ai_generated": true,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = appel(
        &app,
        "POST",
        "/api/v1/profiles/1/favorites/streaming/add",
        Some(json!({
            "item_type": "album", "service": "qobuz", "service_id": "5099749522428",
            "title": "Kind of Blue",
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, liste) = appel(
        &app,
        "GET",
        "/api/v1/profiles/1/favorites/streaming?item_type=album",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{liste}");
    let de = |id: &str| {
        liste
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["service_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} absent de {liste}"))
    };
    assert_eq!(de("tj9je5zd70wsc")["ai_generated"], json!(true), "{liste}");
    assert!(de("5099749522428").get("ai_generated").is_none(), "{liste}");
}
