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
    source: Option<String>,
    source_id: Option<String>,
    output_device_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
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
        .route("/{id}/sleep", get(get_sleep).post(set_sleep))
        .route("/{id}/eq", get(get_eq).post(set_eq))
        .route("/{id}/dsp", post(set_dsp))
        .route("/{id}/crossfade", post(set_crossfade))
        .route("/{id}/normalization", post(set_normalization))
        .route("/{id}/transfer/{target_id}", post(transfer_playback))
        .route("/{id}/alarm", get(get_alarms).post(create_alarm))
        .route("/{id}/alarm/{alarm_id}", axum::routing::delete(delete_alarm))
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

    let output_device_id = body.output_device_id.or_else(|| {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
        zone_repo.get(zone_id).ok().flatten().and_then(|z| z.output_device_id)
    });

    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: Some(target_id),
        source: body.source,
        source_id: body.source_id,
        title: body.title.or_else(|| track.as_ref().map(|t| t.title.clone())),
        artist_name: body.artist_name.or_else(|| track.as_ref().and_then(|t| t.artist_name.clone())),
        album_title: body.album_title.or_else(|| track.as_ref().and_then(|t| t.album_title.clone())),
        cover_url: body.cover_path,
        duration_ms: body.duration_ms.or_else(|| track.as_ref().map(|t| t.duration_ms)),
    };

    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            state.playback.update_queue_info(zone_id, start, track_ids.len() as i64).await;
            let zone_state = state.playback.get_state(zone_id).await;
            Json(json!({
                "zone": zone_state,
                "stream_url": result.stream_url,
                "output_sent": result.output_sent,
                "source": result.source,
            })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn pause(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.pause(zone_id, device_id.as_deref()).await;
    Json(json!({ "status": "paused" }))
}

async fn resume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.resume(zone_id, device_id.as_deref()).await;
    Json(json!({ "status": "playing" }))
}

async fn stop(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.stop(zone_id, device_id.as_deref()).await;
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
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.seek(zone_id, body.position_ms as u64, device_id.as_deref()).await;
    Json(json!({ "position_ms": body.position_ms }))
}

async fn set_volume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<VolumeRequest>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.set_volume(zone_id, body.volume, device_id.as_deref()).await;
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

#[derive(Deserialize)]
struct SleepRequest {
    minutes: u64,
}

async fn set_sleep(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SleepRequest>,
) -> Json<Value> {
    if body.minutes == 0 {
        return Json(json!({ "sleep_timer": null, "zone_id": zone_id }));
    }

    let playback = state.playback.clone();
    let minutes = body.minutes;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(minutes * 60)).await;
        playback.stop(zone_id).await;
    });

    Json(json!({
        "sleep_timer": { "minutes": minutes, "zone_id": zone_id },
    }))
}

async fn get_sleep(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "active": false,
        "remaining_seconds": null,
    }))
}

#[derive(Deserialize)]
struct EqSettings {
    enabled: Option<bool>,
    preset: Option<String>,
    bands: Option<Vec<Value>>,
}

async fn get_eq(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "enabled": false,
        "preset": "flat",
        "bands": [],
    }))
}

async fn set_eq(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<EqSettings>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "enabled": body.enabled.unwrap_or(false),
        "preset": body.preset.unwrap_or_else(|| "custom".into()),
        "bands": body.bands.unwrap_or_default(),
    }))
}

#[derive(Deserialize)]
struct DspSettings {
    crossfeed: Option<String>,
}

async fn set_dsp(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<DspSettings>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "crossfeed": body.crossfeed,
    }))
}

#[derive(Deserialize)]
struct CrossfadeSettings {
    enabled: bool,
    duration: Option<f64>,
}

async fn set_crossfade(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<CrossfadeSettings>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "crossfade_enabled": body.enabled,
        "crossfade_duration": body.duration.unwrap_or(3.0),
    }))
}

#[derive(Deserialize)]
struct NormSettings {
    enabled: bool,
    target_lufs: Option<f64>,
}

async fn set_normalization(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<NormSettings>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "normalization_enabled": body.enabled,
        "target_lufs": body.target_lufs.unwrap_or(-14.0),
    }))
}

async fn transfer_playback(
    State(state): State<AppState>,
    Path((from_zone, target_zone)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let current = state.playback.get_state(from_zone).await;
    if let Some(np) = current.now_playing {
        state.playback.stop(from_zone).await;
        state.playback.play(target_zone, np).await;
        state.playback.set_volume(target_zone, current.volume).await;
        Json(json!({
            "from_zone": from_zone,
            "target_zone": target_zone,
            "status": "transferred",
        })).into_response()
    } else {
        (StatusCode::BAD_REQUEST, "nothing playing to transfer").into_response()
    }
}

async fn get_alarms(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT id, zone_id, time, enabled, days, source_type, source_id, volume, fade_in_seconds FROM alarms WHERE zone_id = ? ORDER BY time")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![zone_id], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "zone_id": row.get::<_, Option<i64>>(1).ok().flatten(),
                    "time": row.get::<_, Option<String>>(2).ok().flatten(),
                    "enabled": row.get::<_, i32>(3).unwrap_or(1) != 0,
                    "days": row.get::<_, Option<String>>(4).ok().flatten(),
                    "source_type": row.get::<_, Option<String>>(5).ok().flatten(),
                    "source_id": row.get::<_, Option<i64>>(6).ok().flatten(),
                    "volume": row.get::<_, Option<f64>>(7).ok().flatten(),
                    "fade_in_seconds": row.get::<_, Option<i32>>(8).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!({ "items": items, "total": items.len() }))
}

#[derive(Deserialize)]
struct CreateAlarm {
    time: String,
    days: Option<String>,
    source_type: Option<String>,
    source_id: Option<i64>,
    volume: Option<f64>,
    fade_in_seconds: Option<i32>,
}

async fn create_alarm(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<CreateAlarm>,
) -> impl IntoResponse {
    match state.db.execute(
        "INSERT INTO alarms (zone_id, time, days, source_type, source_id, volume, fade_in_seconds) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            &zone_id as &dyn rusqlite::types::ToSql,
            &body.time,
            &body.days.unwrap_or_else(|| "1,2,3,4,5,6,7".into()),
            &body.source_type.unwrap_or_else(|| "playlist".into()),
            &body.source_id,
            &body.volume.unwrap_or(0.3),
            &body.fade_in_seconds.unwrap_or(30),
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_alarm(
    State(state): State<AppState>,
    Path((_zone_id, alarm_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    state.db.execute("DELETE FROM alarms WHERE id = ?", &[&alarm_id]).ok();
    StatusCode::NO_CONTENT
}

fn get_zone_device_id(state: &AppState, zone_id: i64) -> Option<String> {
    tune_core::db::zone_repo::ZoneRepo::new(state.db.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id)
}
