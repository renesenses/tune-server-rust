use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::playback::NowPlaying;

use crate::state::AppState;

#[derive(Deserialize)]
struct PlayRequest {
    track_id: Option<i64>,
    track_ids: Option<Vec<i64>>,
    album_id: Option<i64>,
    playlist_id: Option<i64>,
    start_index: Option<i64>,
}

#[derive(Deserialize)]
struct SeekRequest {
    position_ms: i64,
}

#[derive(Deserialize)]
struct VolumeRequest {
    volume: f64,
}

#[derive(Deserialize)]
struct ShuffleQuery {
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct RepeatQuery {
    mode: Option<String>,
}

#[derive(Deserialize)]
struct QueueAddRequest {
    track_ids: Vec<i64>,
    position: Option<i64>,
}

#[derive(Deserialize)]
struct QueueMoveRequest {
    from_position: i64,
    to_position: i64,
}

#[derive(Deserialize)]
struct QueueJumpRequest {
    position: i64,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/now-listening", get(now_listening))
        .route("/{id}/status", get(zone_status))
        .route("/{id}/play", post(play))
        .route("/{id}/pause", post(pause))
        .route("/{id}/resume", post(resume))
        .route("/{id}/stop", post(stop))
        .route("/{id}/next", post(next))
        .route("/{id}/previous", post(previous))
        .route("/{id}/seek", post(seek))
        .route("/{id}/volume", post(set_volume))
        .route("/{id}/shuffle", post(toggle_shuffle))
        .route("/{id}/repeat", post(set_repeat))
        .route("/{id}/queue", get(get_queue))
        .route("/{id}/queue/add", post(queue_add))
        .route("/{id}/queue/move", post(queue_move))
        .route("/{id}/queue/jump", post(queue_jump))
        .route("/{id}/queue/clear", post(queue_clear))
}

async fn now_listening(State(state): State<AppState>) -> Json<Value> {
    let states = state.playback.all_states().await;
    let playing: Vec<Value> = states
        .iter()
        .filter(|s| s.state == tune_core::playback::PlayState::Playing)
        .map(|s| json!(s))
        .collect();
    Json(json!({ "zones": playing }))
}

async fn zone_status(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!(zone_state))
}

async fn play(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<PlayRequest>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::new(state.db.clone());
    let queue_repo = PlayQueueRepo::new(state.db.clone());

    let track_ids: Vec<i64> = if let Some(ids) = body.track_ids {
        ids
    } else if let Some(id) = body.track_id {
        vec![id]
    } else if let Some(album_id) = body.album_id {
        track_repo
            .list_by_album(album_id)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.id)
            .collect()
    } else if let Some(playlist_id) = body.playlist_id {
        tune_core::db::playlist_repo::PlaylistRepo::new(state.db.clone())
            .get_track_ids(playlist_id)
            .unwrap_or_default()
    } else {
        return (StatusCode::BAD_REQUEST, "no track source specified").into_response();
    };

    if track_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to play").into_response();
    }

    queue_repo.set_queue(zone_id, &track_ids).ok();

    let start = body.start_index.unwrap_or(0);
    if start > 0 {
        queue_repo.set_current(zone_id, start).ok();
    }

    let target_id = track_ids.get(start as usize).copied().unwrap_or(track_ids[0]);
    let track = track_repo.get(target_id).ok().flatten();

    let np = NowPlaying {
        track_id: Some(target_id),
        title: track
            .as_ref()
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "Unknown".into()),
        artist_name: track.as_ref().and_then(|t| t.artist_name.clone()),
        album_title: track.as_ref().and_then(|t| t.album_title.clone()),
        cover_path: None,
        duration_ms: track.as_ref().map(|t| t.duration_ms).unwrap_or(0),
        source: "local".into(),
        source_id: None,
        stream_id: None,
    };

    state.playback.play(zone_id, np).await;
    state
        .playback
        .update_queue_info(zone_id, start, track_ids.len() as i64)
        .await;

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!(zone_state)).into_response()
}

async fn pause(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    state.playback.pause(zone_id).await;
    Json(json!({ "status": "paused" }))
}

async fn resume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    state.playback.resume(zone_id).await;
    Json(json!({ "status": "playing" }))
}

async fn stop(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    state.playback.stop(zone_id).await;
    Json(json!({ "status": "stopped" }))
}

async fn next(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;
    let next_pos = current.queue_position + 1;

    if next_pos >= current.queue_length {
        state.playback.stop(zone_id).await;
        return Json(json!({ "status": "stopped", "reason": "end_of_queue" })).into_response();
    }

    let queue_repo = PlayQueueRepo::new(state.db.clone());
    queue_repo.set_current(zone_id, next_pos).ok();

    let queue = queue_repo.get_queue(zone_id).unwrap_or_default();
    if let Some(item) = queue.iter().find(|i| i.is_current) {
        let np = NowPlaying {
            track_id: Some(item.track_id),
            title: item.title.clone().unwrap_or_default(),
            artist_name: item.artist_name.clone(),
            album_title: item.album_title.clone(),
            cover_path: item.cover_path.clone(),
            duration_ms: item.duration_ms.unwrap_or(0),
            source: "local".into(),
            source_id: None,
            stream_id: None,
        };
        state.playback.play(zone_id, np).await;
        state
            .playback
            .update_queue_info(zone_id, next_pos, current.queue_length)
            .await;
    }

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!(zone_state)).into_response()
}

async fn previous(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;

    if current.position_ms > 3000 {
        state.playback.seek(zone_id, 0).await;
        return Json(json!({ "status": "restarted" })).into_response();
    }

    let prev_pos = (current.queue_position - 1).max(0);
    let queue_repo = PlayQueueRepo::new(state.db.clone());
    queue_repo.set_current(zone_id, prev_pos).ok();

    let queue = queue_repo.get_queue(zone_id).unwrap_or_default();
    if let Some(item) = queue.iter().find(|i| i.is_current) {
        let np = NowPlaying {
            track_id: Some(item.track_id),
            title: item.title.clone().unwrap_or_default(),
            artist_name: item.artist_name.clone(),
            album_title: item.album_title.clone(),
            cover_path: item.cover_path.clone(),
            duration_ms: item.duration_ms.unwrap_or(0),
            source: "local".into(),
            source_id: None,
            stream_id: None,
        };
        state.playback.play(zone_id, np).await;
        state
            .playback
            .update_queue_info(zone_id, prev_pos, current.queue_length)
            .await;
    }

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!(zone_state)).into_response()
}

async fn seek(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SeekRequest>,
) -> Json<Value> {
    state.playback.seek(zone_id, body.position_ms).await;
    Json(json!({ "position_ms": body.position_ms }))
}

async fn set_volume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<VolumeRequest>,
) -> Json<Value> {
    state.playback.set_volume(zone_id, body.volume).await;
    Json(json!({ "volume": body.volume }))
}

async fn toggle_shuffle(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(q): Query<ShuffleQuery>,
) -> Json<Value> {
    let current = state.playback.get_state(zone_id).await;
    let enabled = q.enabled.unwrap_or(!current.shuffle);
    state.playback.set_shuffle(zone_id, enabled).await;
    Json(json!({ "shuffle": enabled }))
}

async fn set_repeat(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(q): Query<RepeatQuery>,
) -> Json<Value> {
    let mode = match q.mode.as_deref() {
        Some("one") => tune_core::playback::RepeatMode::One,
        Some("all") => tune_core::playback::RepeatMode::All,
        _ => tune_core::playback::RepeatMode::Off,
    };
    state.playback.set_repeat(zone_id, mode).await;
    Json(json!({ "repeat": mode }))
}

async fn get_queue(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let queue_repo = PlayQueueRepo::new(state.db);
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    let current = items.iter().find(|i| i.is_current).cloned();
    Json(json!({ "items": items, "current": current, "total": items.len() }))
}

async fn queue_add(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueAddRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::new(state.db);
    match queue_repo.add_tracks(zone_id, &body.track_ids, body.position) {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({ "added": body.track_ids.len() })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn queue_move(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueMoveRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::new(state.db);
    let mut items = queue_repo.get_queue(zone_id).unwrap_or_default();
    let from = body.from_position as usize;
    let to = body.to_position as usize;

    if from < items.len() && to < items.len() {
        let item = items.remove(from);
        items.insert(to, item);
        let track_ids: Vec<i64> = items.iter().map(|i| i.track_id).collect();
        queue_repo.set_queue(zone_id, &track_ids).ok();
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::BAD_REQUEST, "position out of range").into_response()
    }
}

async fn queue_jump(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueJumpRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::new(state.db.clone());
    queue_repo.set_current(zone_id, body.position).ok();

    let queue = queue_repo.get_queue(zone_id).unwrap_or_default();
    if let Some(item) = queue.iter().find(|i| i.is_current) {
        let np = NowPlaying {
            track_id: Some(item.track_id),
            title: item.title.clone().unwrap_or_default(),
            artist_name: item.artist_name.clone(),
            album_title: item.album_title.clone(),
            cover_path: item.cover_path.clone(),
            duration_ms: item.duration_ms.unwrap_or(0),
            source: "local".into(),
            source_id: None,
            stream_id: None,
        };
        state.playback.play(zone_id, np).await;
        state
            .playback
            .update_queue_info(zone_id, body.position, queue.len() as i64)
            .await;
    }

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!(zone_state)).into_response()
}

async fn queue_clear(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::new(state.db);
    queue_repo.clear(zone_id).ok();
    state.playback.stop(zone_id).await;
    StatusCode::NO_CONTENT
}
