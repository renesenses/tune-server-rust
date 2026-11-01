//! #7335 (laguespa, fil forum 2510, rc8.1 Linux) — « Créer » une collection
//! ne la fait pas apparaître.
//!
//! Chaque route qui modifie les dossiers réécrivait le réglage `collections`
//! par `settings.set(…).ok()` : l'écriture refusée par la base était avalée,
//! et la route répondait quand même `201 Created` (ou `200`, `204`) avec le
//! dossier dans le corps. Le client croyait le dossier créé ; l'écran, relu,
//! ne le montrait pas.
//!
//! La base est rendue réfractaire à l'écriture du SEUL réglage `collections`
//! par un déclencheur SQLite : le reste du serveur marche, exactement comme
//! une écriture qui échoue en production (base verrouillée, disque plein,
//! droits). Le témoin porte sur le code HTTP ET sur le fait relu.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn banc() -> (tune_server::state::AppState, axum::Router) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// Toute écriture du réglage `collections` est refusée à partir d'ici.
fn refuser_l_ecriture(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch(
            "CREATE TRIGGER refus_collections_ins BEFORE INSERT ON settings
               WHEN NEW.key = 'collections'
               BEGIN SELECT RAISE(ABORT, 'disque plein (banc #7335)'); END;
             CREATE TRIGGER refus_collections_maj BEFORE UPDATE ON settings
               WHEN NEW.key = 'collections'
               BEGIN SELECT RAISE(ABORT, 'disque plein (banc #7335)'); END;",
        )
        .unwrap();
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
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

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn avec_corps(
    app: &axum::Router,
    methode: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method(methode)
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

fn noms(liste: &Value) -> Vec<String> {
    liste
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Le chemin heureux, relu par la LISTE qui alimente l'écran Collections.
#[tokio::test]
async fn creation_relue_dans_la_liste_7335() {
    let (_state, app) = banc();
    let (st, cree) = avec_corps(
        &app,
        "POST",
        "/api/v1/library/collections",
        json!({"name": "test", "description": "test"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    let (st, liste) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK, "liste: {liste}");
    assert_eq!(noms(&liste), vec!["test".to_string()]);
}

/// Le cœur du ticket : une création que la base refuse ne répond plus 201.
#[tokio::test]
async fn creation_refusee_par_la_base_ne_repond_plus_201_7335() {
    let (state, app) = banc();
    refuser_l_ecriture(&state);
    let (st, corps) = avec_corps(
        &app,
        "POST",
        "/api/v1/library/collections",
        json!({"name": "test", "description": "test"}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "#7335 : la base a refusé l'écriture et la route répond {st} avec {corps} — le client croit le dossier créé"
    );
    let message = corps["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("refusé l'écriture"),
        "le message doit dire ce qui s'est passé: {corps}"
    );
    let (_, liste) = get(&app, "/api/v1/library/collections").await;
    assert!(noms(&liste).is_empty(), "rien n'a été écrit: {liste}");
}

/// Même `.ok()` sur la modification, la suppression, le rangement et le
/// retrait d'un album : aucun ne répond plus « fait » quand rien n'est fait.
#[tokio::test]
async fn modifier_supprimer_ranger_refuses_ne_repondent_plus_ok_7335() {
    let (state, app) = banc();
    let (st, cree) = avec_corps(
        &app,
        "POST",
        "/api/v1/library/collections",
        json!({"name": "Jazz"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    let id = cree["id"].as_i64().unwrap();
    refuser_l_ecriture(&state);

    let (st, corps) = avec_corps(
        &app,
        "PUT",
        &format!("/api/v1/library/collections/{id}"),
        json!({"name": "Jazz du soir"}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "PUT refusé rendu {st}: {corps}"
    );

    let (st, corps) = send(
        &app,
        Request::delete(format!("/api/v1/library/collections/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "DELETE refusé rendu {st}: {corps}"
    );

    let (st, corps) = send(
        &app,
        Request::post(format!("/api/v1/library/collections/{id}/albums/42"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "rangement refusé rendu {st}: {corps}"
    );

    let (st, corps) = send(
        &app,
        Request::delete(format!("/api/v1/library/collections/{id}/albums/42"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::INTERNAL_SERVER_ERROR,
        "retrait refusé rendu {st}: {corps}"
    );

    // Le dossier relu est intact : ni renommé, ni supprimé.
    let (_, liste) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(noms(&liste), vec!["Jazz".to_string()], "liste: {liste}");
}
