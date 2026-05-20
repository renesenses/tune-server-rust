use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::playback::NowPlaying;

use crate::state::AppState;

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct CreateRadio {
    name: String,
    url: String,
    homepage: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    language: Option<String>,
    genre: Option<String>,
    codec: Option<String>,
    bitrate: Option<i32>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_radios).post(create_radio))
        .route("/search", get(search_radios))
        .route("/favorites", get(list_favorites))
        .route("/{id}", get(get_radio).delete(delete_radio))
        .route("/{id}/favorite", post(toggle_favorite))
        .route("/{id}/play/{zone_id}", post(play_radio))
}

async fn list_radios(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::new(state.db);
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn get_radio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_radio(
    State(state): State<AppState>,
    Json(body): Json<CreateRadio>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let station = RadioStation {
        id: None,
        name: body.name,
        url: body.url,
        homepage: body.homepage,
        logo_url: body.logo_url,
        country: body.country,
        language: body.language,
        genre: body.genre,
        codec: body.codec,
        bitrate: body.bitrate,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    match repo.create(&station) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn play_radio(
    State(state): State<AppState>,
    Path((id, zone_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db.clone());
    let Some(radio) = repo.get(id).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "radio not found").into_response();
    };

    let device_id = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);

    let np = NowPlaying {
        track_id: None,
        title: radio.name.clone(),
        artist_name: Some("Live Radio".into()),
        album_title: Some("Live Radio".into()),
        cover_path: radio.logo_url.clone(),
        duration_ms: 0,
        source: "radio".into(),
        source_id: Some(id.to_string()),
        stream_id: None,
    };
    state.playback.play(zone_id, np).await;

    let output_sent = if let Some(ref did) = device_id {
        let outputs = state.outputs.lock().await;
        if let Some(output) = outputs.get(did) {
            let output = output.lock().await;
            output
                .play_url(&radio.url, "audio/aac", Some(&radio.name), None)
                .await
                .is_ok()
        } else {
            false
        }
    } else {
        false
    };

    repo.record_play(id).ok();

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({
        "zone_id": zone_id,
        "radio": radio.name,
        "output_sent": output_sent,
        "state": zone_state,
    }))
    .into_response()
}

async fn search_radios(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let repo = RadioRepo::new(state.db);
    let items = repo.search(&q.q).unwrap_or_default();
    Json(json!(items))
}

async fn list_favorites(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::new(state.db);
    let items = repo.favorites().unwrap_or_default();
    Json(json!(items))
}

#[derive(Deserialize)]
struct FavoriteToggle {
    favorite: Option<bool>,
}

async fn toggle_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<FavoriteToggle>>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let current = repo.get(id).ok().flatten();
    let Some(current) = current else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let new_state = body
        .and_then(|b| b.favorite)
        .unwrap_or(!current.is_favorite);
    match repo.set_favorite(id, new_state) {
        Ok(_) => Json(json!({ "is_favorite": new_state })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
