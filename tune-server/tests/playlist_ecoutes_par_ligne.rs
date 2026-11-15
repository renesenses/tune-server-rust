//! Recette rc12 du 15/11 (Bertrand, sur le .18) : « Playlists : # nb
//! d'écoutes et last played sans valeur !! ».
//!
//! La fiche d'une playlist montre les colonnes « # écoutes » et « Dernière
//! écoute » (`play_count`, `last_played_at`), comme la liste des pistes d'un
//! album. `GET /playlists/{id}/tracks` ne les posait sur aucune ligne : les
//! deux cellules restaient vides partout.
//!
//! Ce banc passe par la ROUTE MONTÉE, avec une écoute en base :
//! - ligne locale jouée → son compte et sa dernière écoute ;
//! - ligne locale jamais jouée → `0` / `null` ;
//! - ligne de service (Qobuz) jouée → les écoutes de ce serveur sur son
//!   `source_id`.
//!
//! Contre-épreuve : retirez `attacher_ecoutes_des_lignes(&state, &mut items)`
//! de `get_tracks` et les trois essais passent au rouge.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn piste(state: &tune_server::state::AppState, titre: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(format!("/pl-ecoutes/{titre}.flac"));
    repo.create(&t).expect("insert track")
}

fn ecoute_locale(state: &tune_server::state::AppState, track_id: i64, titre: &str, quand: &str) {
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO listen_history (track_id, title, source, listened_at) \
                 VALUES ({track_id}, '{titre}', 'local', '{quand}')"
            ),
            &[],
        )
        .expect("ecoute locale");
}

fn ecoute_de_service(state: &tune_server::state::AppState, source_id: &str, quand: &str) {
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO listen_history (track_id, title, source, source_id, listened_at) \
                 VALUES (NULL, 'Qobuz', 'qobuz', '{source_id}', '{quand}')"
            ),
            &[],
        )
        .expect("ecoute de service");
}

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
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

/// Une playlist : « Jouée », « Jamais jouée » (locales), puis une ligne Qobuz.
async fn lignes() -> Vec<Value> {
    let state = etat();
    let jouee = piste(&state, "Jouee");
    let jamais = piste(&state, "Jamais");
    ecoute_locale(&state, jouee, "Jouee", "2026-11-01T10:00:00Z");
    ecoute_locale(&state, jouee, "Jouee", "2026-11-14T21:30:00Z");
    ecoute_de_service(&state, "q-ecoutes", "2026-11-12T08:00:00Z");
    let app = tune_server::routes::router(state);

    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/playlists",
        Some(json!({"name": "ecoutes"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    let (st, v) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        Some(json!({
            "track_ids": [jouee, jamais],
            "streaming_tracks": [{"source": "qobuz", "source_id": "q-ecoutes", "title": "Qobuz"}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");

    let (st, v) = appel(&app, "GET", &format!("/api/v1/playlists/{id}/tracks"), None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let lignes = v.as_array().cloned().unwrap();
    assert_eq!(lignes.len(), 3, "{v}");
    lignes
}

fn ligne<'a>(lignes: &'a [Value], titre: &str) -> &'a Value {
    lignes
        .iter()
        .find(|l| l["title"] == titre)
        .unwrap_or_else(|| panic!("ligne {titre} absente : {lignes:?}"))
}

#[tokio::test]
async fn une_ligne_locale_jouee_porte_son_compte_et_sa_derniere_ecoute() {
    let lignes = lignes().await;
    let l = ligne(&lignes, "Jouee");
    assert_eq!(l["play_count"], 2, "{l}");
    assert_eq!(l["last_played_at"], "2026-11-14T21:30:00Z", "{l}");
}

#[tokio::test]
async fn une_ligne_locale_jamais_jouee_porte_zero_et_null() {
    let lignes = lignes().await;
    let l = ligne(&lignes, "Jamais");
    assert_eq!(l.get("play_count"), Some(&json!(0)), "{l}");
    assert_eq!(l.get("last_played_at"), Some(&Value::Null), "{l}");
}

#[tokio::test]
async fn une_ligne_de_service_jouee_porte_les_ecoutes_de_ce_serveur() {
    let lignes = lignes().await;
    let l = ligne(&lignes, "Qobuz");
    assert_eq!(l["play_count"], 1, "{l}");
    assert_eq!(l["last_played_at"], "2026-11-12T08:00:00Z", "{l}");
}
