use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::orchestrator::PlayResult;

use crate::error::AppError;
use crate::state::AppState;

/// Persist the queue state for a zone to disk (non-blocking).
fn persist_queue_async(state: &AppState, zone_id: i64) {
    let db = state.backend.clone();
    let db_path = state.config.db_path.clone();
    let playback = state.playback.clone();
    tokio::spawn(async move {
        let zone_state = playback.get_state(zone_id).await;
        tokio::task::spawn_blocking(move || {
            tune_core::queue_persistence::save_queue(&db, &db_path, zone_id, &zone_state);
        });
    });
}

async fn build_zone_json(state: &AppState, zone_id: i64) -> Value {
    let zone_state = state.playback.get_state(zone_id).await;
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_db = zone_repo.get(zone_id).ok().flatten();
    let mut v = json!({
        "id": zone_id,
        "name": zone_db.as_ref().map(|z| &z.name),
        "output_type": zone_db.as_ref().and_then(|z| z.output_type.as_ref()),
        "output_device_id": zone_db.as_ref().and_then(|z| z.output_device_id.as_ref()),
        "volume": zone_state.volume,
        "state": zone_state.state,
        "current_track": zone_state.now_playing.as_ref().map(|np| json!({
            "id": np.track_id,
            "title": np.title,
            "artist_name": np.artist_name,
            "album_title": np.album_title,
            "cover_path": np.cover_path,
            "duration_ms": np.duration_ms,
            "source": np.source,
            "source_id": np.source_id,
        })),
        "position_ms": zone_state.position_ms,
        "queue_length": zone_state.queue_length,
        "queue_position": zone_state.queue_position,
        "muted": zone_state.muted,
    });
    // Include stream_url for browser playback zones so the web client
    // can feed it to an HTML5 <audio> element.
    if let Some(ref np) = zone_state.now_playing {
        if let Some(ref stream_id) = np.stream_id {
            let server_ip = state.config.advertised_ip.clone().unwrap_or_else(|| {
                tune_core::discovery::ssdp::get_local_ip()
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "127.0.0.1".into())
            });
            let ext = "flac";
            let stream_url = format!(
                "http://{}:{}/stream/{}.{}",
                server_ip, state.port, stream_id, ext
            );
            v.as_object_mut()
                .unwrap()
                .insert("stream_url".into(), json!(stream_url));
        }
    }
    v
}

async fn build_zone_json_with_result(state: &AppState, zone_id: i64, result: &PlayResult) -> Value {
    let mut zone = build_zone_json(state, zone_id).await;
    if let Some(ref err) = result.error {
        zone.as_object_mut()
            .unwrap()
            .insert("error".into(), json!(err));
    }
    zone.as_object_mut()
        .unwrap()
        .insert("output_sent".into(), json!(result.output_sent));
    // Expose stream_url for browser playback zones
    if let Some(ref url) = result.stream_url {
        zone.as_object_mut()
            .unwrap()
            .insert("stream_url".into(), json!(url));
    }
    zone
}

#[derive(Deserialize)]
struct PlayRequest {
    track_id: Option<i64>,
    track_ids: Option<Vec<i64>>,
    album_id: Option<i64>,
    playlist_id: Option<i64>,
    start_index: Option<i64>,
    source: Option<String>,
    source_id: Option<String>,
    streaming_album_id: Option<String>,
    streaming_playlist_id: Option<String>,
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
    #[serde(default)]
    track_ids: Vec<i64>,
    position: Option<i64>,
    // Streaming track fields
    source: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
}

#[derive(Deserialize)]
struct SaveAsPlaylistRequest {
    name: Option<String>,
}

#[derive(Deserialize)]
struct QueueMoveRequest {
    from_position: i64,
    to_position: i64,
}

#[derive(Deserialize)]
struct TransferRequest {
    target_zone_id: i64,
    #[serde(default = "default_true")]
    stop_source: bool,
}

fn default_true() -> bool {
    true
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
        .route("/{id}/queue", get(get_queue).delete(queue_clear))
        .route("/{id}/queue/add", post(queue_add))
        .route("/{id}/queue/move", post(queue_move))
        .route("/{id}/queue/jump", post(queue_jump))
        .route("/{id}/queue/clear", post(queue_clear))
        .route(
            "/{id}/queue/{position}",
            axum::routing::delete(queue_remove),
        )
        .route("/{id}/queue/save-as-playlist", post(save_queue_as_playlist))
        .route("/{id}/sleep", get(get_sleep).post(set_sleep))
        .route("/{id}/eq", get(get_eq).post(set_eq))
        .route("/{id}/dsp", post(set_dsp))
        .route("/{id}/crossfade", post(set_crossfade))
        .route("/{id}/normalization", post(set_normalization))
        .route("/{id}/transfer/{target_id}", post(transfer_playback))
        .route("/{id}/transfer", post(transfer_queue))
        .route("/{id}/alarm", get(get_alarms).post(create_alarm))
        .route(
            "/{id}/alarm/{alarm_id}",
            axum::routing::delete(delete_alarm),
        )
        .route("/{id}/pins", get(get_zone_pins).post(set_zone_pin))
        .route("/{id}/pins/{index}", axum::routing::delete(clear_zone_pin))
        .route("/{id}/pins/{index}/invoke", post(invoke_zone_pin))
        .route("/{id}/pins/from-queue", post(save_queue_as_pin))
        .route("/{id}/audiophile", get(get_audiophile).post(set_audiophile))
        .route("/{id}/quality", get(get_quality).post(set_quality))
        .route("/{id}/share", post(share_now_playing))
        .route(
            "/{id}/audio-profile",
            get(get_audio_profile).post(set_audio_profile),
        )
}

async fn now_listening(State(state): State<AppState>) -> Json<Value> {
    let states = state.playback.all_states().await;
    let playing: Vec<Value> = states
        .iter()
        .filter(|s| s.state == tune_core::playback::PlayState::Playing)
        .map(|s| json!(s))
        .collect();
    Json(json!(playing))
}

async fn zone_status(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let zone_state = state.playback.get_state(zone_id).await;
    let mut v = serde_json::to_value(&zone_state).unwrap_or_default();
    if let Some(track_id) = zone_state.now_playing.as_ref().and_then(|np| np.track_id) {
        let credits = TrackRepo::with_backend(state.backend.clone())
            .get_credits(track_id)
            .unwrap_or_default();
        if !credits.is_empty() {
            if let Some(np) = v.get_mut("now_playing").and_then(|np| np.as_object_mut()) {
                np.insert(
                    "credits".into(),
                    serde_json::to_value(&credits).unwrap_or_default(),
                );
            }
        }
    }
    // Expose stream_url for browser playback zones
    if let Some(ref np) = zone_state.now_playing {
        if let Some(ref stream_id) = np.stream_id {
            let server_ip = state.config.advertised_ip.clone().unwrap_or_else(|| {
                tune_core::discovery::ssdp::get_local_ip()
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "127.0.0.1".into())
            });
            let ext = "flac"; // default extension
            let stream_url = format!(
                "http://{}:{}/stream/{}.{}",
                server_ip, state.port, stream_id, ext
            );
            v.as_object_mut()
                .unwrap()
                .insert("stream_url".into(), json!(stream_url));
        }
    }
    Json(v)
}

async fn play(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    body: Option<Json<PlayRequest>>,
) -> impl IntoResponse {
    // When called with an empty body (e.g. Play after Stop), resume the
    // current track instead of returning 400 "no track source specified".
    let body = match body {
        Some(Json(b)) => b,
        None => {
            let current = state.playback.get_state(zone_id).await;
            if let Some(ref np) = current.now_playing {
                let output_device_id = get_zone_device_id(&state, zone_id);
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: np.track_id,
                    source: if np.source == "local" {
                        None
                    } else {
                        Some(np.source.clone())
                    },
                    source_id: np.source_id.clone(),
                    title: Some(np.title.clone()),
                    artist_name: np.artist_name.clone(),
                    album_title: np.album_title.clone(),
                    cover_url: np.cover_path.clone(),
                    duration_ms: Some(np.duration_ms),
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        // Restore queue_length from DB so the poller can
                        // advance tracks (fixes repeat-all after restart).
                        let qr = PlayQueueRepo::with_backend(state.backend.clone());
                        let local_c = qr.count(zone_id).unwrap_or(0);
                        let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                        let q_len = if local_c > 0 { local_c } else { stream_c };
                        if q_len > 0 {
                            let cur_pos = state.playback.get_state(zone_id).await.queue_position;
                            state
                                .playback
                                .update_queue_info(zone_id, cur_pos, q_len)
                                .await;
                        }
                        persist_queue_async(&state, zone_id);
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                };
            }
            return (
                StatusCode::BAD_REQUEST,
                "no track source specified and nothing to resume",
            )
                .into_response();
        }
    };

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // --- Streaming album: fetch tracks from the service, queue them, play first ---
    if let (Some(source), Some(album_id)) = (&body.source, &body.streaming_album_id) {
        let registry = state.services.lock().await;
        let svc = match registry.get(source) {
            Some(s) => s,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown service: {source}"),
                )
                    .into_response();
            }
        };
        let svc = svc.lock().await;
        let tracks = match svc.get_album_tracks(album_id).await {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        };
        drop(svc);
        drop(registry);

        if tracks.is_empty() {
            return (StatusCode::BAD_REQUEST, "album has no tracks").into_response();
        }

        let start = body.start_index.unwrap_or(0) as usize;
        let start = start.min(tracks.len() - 1);
        let first = &tracks[start];

        let output_device_id = body.output_device_id.clone().or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });

        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source.clone()),
            source_id: Some(first.id.clone()),
            title: Some(first.title.clone()),
            artist_name: Some(first.artist.clone()),
            album_title: first.album.clone(),
            cover_url: first.cover_path.clone(),
            duration_ms: Some(first.duration_ms as i64),
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                let queue_items: Vec<_> = tracks
                    .iter()
                    .map(|t| {
                        (
                            t.id.clone(),
                            t.title.clone(),
                            t.artist.clone(),
                            t.album.clone(),
                            t.cover_path.clone(),
                            t.duration_ms as i64,
                            Some(source.clone()),
                        )
                    })
                    .collect();
                if let Err(e) = queue_repo.set_streaming_queue(zone_id, &queue_items) {
                    warn!(zone_id, error = %e, "set_streaming_queue_failed");
                }
                state
                    .playback
                    .update_queue_info(zone_id, start as i64, tracks.len() as i64)
                    .await;
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }

    // --- Streaming playlist: fetch tracks from the service, queue them, play first ---
    if let (Some(source), Some(playlist_id)) = (&body.source, &body.streaming_playlist_id) {
        let registry = state.services.lock().await;
        let svc = match registry.get(source) {
            Some(s) => s,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown service: {source}"),
                )
                    .into_response();
            }
        };
        let svc = svc.lock().await;
        let tracks = match svc.get_playlist_tracks(playlist_id).await {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        };
        drop(svc);
        drop(registry);

        if tracks.is_empty() {
            return (StatusCode::BAD_REQUEST, "playlist has no tracks").into_response();
        }

        let start = body.start_index.unwrap_or(0) as usize;
        let start = start.min(tracks.len() - 1);
        let first = &tracks[start];

        let output_device_id = body.output_device_id.clone().or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });

        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source.clone()),
            source_id: Some(first.id.clone()),
            title: Some(first.title.clone()),
            artist_name: Some(first.artist.clone()),
            album_title: first.album.clone(),
            cover_url: first.cover_path.clone(),
            duration_ms: Some(first.duration_ms as i64),
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                let queue_items: Vec<_> = tracks
                    .iter()
                    .map(|t| {
                        (
                            t.id.clone(),
                            t.title.clone(),
                            t.artist.clone(),
                            t.album.clone(),
                            t.cover_path.clone(),
                            t.duration_ms as i64,
                            Some(source.clone()),
                        )
                    })
                    .collect();
                if let Err(e) = queue_repo.set_streaming_queue(zone_id, &queue_items) {
                    warn!(zone_id, error = %e, "set_streaming_queue_failed");
                }
                state
                    .playback
                    .update_queue_info(zone_id, start as i64, tracks.len() as i64)
                    .await;
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }

    // --- Single streaming track (source + source_id, no track_id/track_ids) ---
    if body.source.is_some()
        && body.source_id.is_some()
        && body.track_id.is_none()
        && body.track_ids.is_none()
    {
        let source_id_val = body.source_id.clone().unwrap_or_default();
        let source_for_q = body.source.clone();
        let title_val = body.title.clone().unwrap_or_default();
        let artist_val = body.artist_name.clone().unwrap_or_default();
        let album_val = body.album_title.clone();
        let cover_val = body.cover_path.clone();
        let duration_val = body.duration_ms.unwrap_or(0);

        let output_device_id = body.output_device_id.or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });
        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: body.source,
            source_id: body.source_id,
            title: body.title,
            artist_name: body.artist_name,
            album_title: body.album_title,
            cover_url: body.cover_path,
            duration_ms: body.duration_ms,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                // Persist single streaming track to DB so GET /queue returns it
                let queue_item = vec![(
                    source_id_val,
                    title_val,
                    artist_val,
                    album_val,
                    cover_val,
                    duration_val,
                    source_for_q,
                )];
                if let Err(e) = queue_repo.set_streaming_queue(zone_id, &queue_item) {
                    warn!(zone_id, error = %e, "set_streaming_queue_failed");
                }
                state.playback.update_queue_info(zone_id, 0, 1).await;
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }

    // Resolve track list: containers (album/playlist) take priority so the full
    // collection is always queued, even when a track_id is also provided.
    let track_ids: Vec<i64> = if let Some(album_id) = body.album_id {
        track_repo
            .list_by_album(album_id)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.id)
            .collect()
    } else if let Some(playlist_id) = body.playlist_id {
        tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
            .get_track_ids(playlist_id)
            .unwrap_or_default()
    } else if let Some(ids) = body.track_ids {
        ids
    } else if let Some(id) = body.track_id {
        vec![id]
    } else {
        // No track source specified — try to resume the current track.
        // This handles the case where the user presses Play after Stop:
        // the web/Flutter client sends POST /play with an empty body.
        let current = state.playback.get_state(zone_id).await;
        if let Some(ref np) = current.now_playing {
            let output_device_id = body
                .output_device_id
                .or_else(|| get_zone_device_id(&state, zone_id));
            let orch_req = tune_core::orchestrator::PlayRequest {
                zone_id,
                output_device_id,
                track_id: np.track_id,
                source: if np.source == "local" {
                    None
                } else {
                    Some(np.source.clone())
                },
                source_id: np.source_id.clone(),
                title: Some(np.title.clone()),
                artist_name: np.artist_name.clone(),
                album_title: np.album_title.clone(),
                cover_url: np.cover_path.clone(),
                duration_ms: Some(np.duration_ms),
            };
            return match state.orchestrator.play(orch_req).await {
                Ok(result) => {
                    persist_queue_async(&state, zone_id);
                    Json(build_zone_json_with_result(&state, zone_id, &result).await)
                        .into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
        return (StatusCode::BAD_REQUEST, "no track source specified").into_response();
    };

    if track_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to play").into_response();
    }

    if let Err(e) = queue_repo.set_queue(zone_id, &track_ids) {
        warn!(zone_id, error = %e, "set_queue_failed");
    }

    // When a container (album/playlist) is requested alongside a track_id,
    // infer start_index from the position of that track in the resolved list.
    let start = body.start_index.unwrap_or_else(|| {
        body.track_id
            .and_then(|tid| track_ids.iter().position(|&id| id == tid))
            .map(|pos| pos as i64)
            .unwrap_or(0)
    });
    if start > 0 {
        queue_repo.set_current(zone_id, start).ok();
    }

    let target_id = track_ids
        .get(start as usize)
        .copied()
        .unwrap_or(track_ids[0]);
    let track = track_repo.get(target_id).ok().flatten();

    let output_device_id = body.output_device_id.or_else(|| {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        zone_repo
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    });

    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: Some(target_id),
        source: body.source,
        source_id: body.source_id,
        title: body
            .title
            .or_else(|| track.as_ref().map(|t| t.title.clone())),
        artist_name: body
            .artist_name
            .or_else(|| track.as_ref().and_then(|t| t.artist_name.clone())),
        album_title: body
            .album_title
            .or_else(|| track.as_ref().and_then(|t| t.album_title.clone())),
        cover_url: body
            .cover_path
            .or_else(|| track.as_ref().and_then(|t| t.cover_path.clone())),
        duration_ms: body
            .duration_ms
            .or_else(|| track.as_ref().map(|t| t.duration_ms)),
    };

    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            state
                .playback
                .update_queue_info(zone_id, start, track_ids.len() as i64)
                .await;
            persist_queue_async(&state, zone_id);
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn pause(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state
        .orchestrator
        .pause(zone_id, device_id.as_deref())
        .await;
    Json(build_zone_json(&state, zone_id).await)
}

async fn resume(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;

    // When stopped with a valid NowPlaying, re-play the current track from the start
    if current.state == tune_core::playback::PlayState::Stopped {
        if let Some(ref np) = current.now_playing {
            let output_device_id = get_zone_device_id(&state, zone_id);
            let orch_req = tune_core::orchestrator::PlayRequest {
                zone_id,
                output_device_id,
                track_id: np.track_id,
                source: if np.source == "local" {
                    None
                } else {
                    Some(np.source.clone())
                },
                source_id: np.source_id.clone(),
                title: Some(np.title.clone()),
                artist_name: np.artist_name.clone(),
                album_title: np.album_title.clone(),
                cover_url: np.cover_path.clone(),
                duration_ms: Some(np.duration_ms),
            };
            return match state.orchestrator.play(orch_req).await {
                Ok(result) => {
                    // Restore queue_length from DB so the poller can
                    // advance tracks (fixes repeat-all after restart).
                    let qr = PlayQueueRepo::with_backend(state.backend.clone());
                    let local_c = qr.count(zone_id).unwrap_or(0);
                    let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                    let q_len = if local_c > 0 { local_c } else { stream_c };
                    if q_len > 0 {
                        let cur_pos = state.playback.get_state(zone_id).await.queue_position;
                        state
                            .playback
                            .update_queue_info(zone_id, cur_pos, q_len)
                            .await;
                    }
                    Json(build_zone_json_with_result(&state, zone_id, &result).await)
                        .into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            };
        }
    }

    // For a normal resume (paused → playing), also ensure queue_length is
    // populated — it may be zero after a server restart.
    {
        let qr = PlayQueueRepo::with_backend(state.backend.clone());
        let local_c = qr.count(zone_id).unwrap_or(0);
        let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
        let q_len = if local_c > 0 { local_c } else { stream_c };
        if q_len > 0 {
            let cur_pos = state.playback.get_state(zone_id).await.queue_position;
            state
                .playback
                .update_queue_info(zone_id, cur_pos, q_len)
                .await;
        }
    }

    let device_id = get_zone_device_id(&state, zone_id);
    state
        .orchestrator
        .resume(zone_id, device_id.as_deref())
        .await;
    Json(build_zone_json(&state, zone_id).await).into_response()
}

async fn stop(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.stop(zone_id, device_id.as_deref()).await;
    Json(build_zone_json(&state, zone_id).await)
}

async fn next(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;

    let Some(next_pos) = tune_core::poller::PositionPoller::next_position(&current) else {
        let device_id = get_zone_device_id(&state, zone_id);
        state.orchestrator.stop(zone_id, device_id.as_deref()).await;
        return Json(json!({ "status": "stopped", "reason": "end_of_queue" })).into_response();
    };

    let s = state.clone();
    tokio::spawn(async move {
        if let Err(e) = s.orchestrator.play_from_queue(zone_id, next_pos).await {
            tracing::warn!(zone_id, error = %e, "next_play_failed");
        }
    });

    Json(json!({ "status": "playing", "queue_position": next_pos })).into_response()
}

async fn previous(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;

    if current.position_ms > 3000 {
        let device_id = get_zone_device_id(&state, zone_id);
        state
            .orchestrator
            .seek(zone_id, 0, device_id.as_deref())
            .await;
        return Json(json!({ "status": "restarted" })).into_response();
    }

    let prev_pos = (current.queue_position - 1).max(0);

    let s = state.clone();
    tokio::spawn(async move {
        if let Err(e) = s.orchestrator.play_from_queue(zone_id, prev_pos).await {
            tracing::warn!(zone_id, error = %e, "prev_play_failed");
        }
    });

    Json(json!({ "status": "playing", "queue_position": prev_pos })).into_response()
}

async fn seek(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SeekRequest>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state
        .orchestrator
        .seek(zone_id, body.position_ms as u64, device_id.as_deref())
        .await;
    Json(json!({ "position_ms": body.position_ms }))
}

async fn set_volume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<VolumeRequest>,
) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state
        .orchestrator
        .set_volume(zone_id, body.volume, device_id.as_deref())
        .await;
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
    persist_queue_async(&state, zone_id);
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
    persist_queue_async(&state, zone_id);
    Json(json!({ "repeat": mode }))
}

async fn get_queue(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    if !items.is_empty() {
        let position = items.iter().position(|i| i.is_current).unwrap_or(0);
        let length = items.len();
        return Json(json!({ "tracks": items, "position": position, "length": length }));
    }
    let streaming_items = queue_repo.get_streaming_queue(zone_id).unwrap_or_default();
    let ps = state.playback.get_state(zone_id).await;
    Json(
        json!({ "tracks": streaming_items, "position": ps.queue_position, "length": streaming_items.len() }),
    )
}

async fn queue_add(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueAddRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // --- Streaming track: add to streaming queue ---
    if let (Some(source), Some(source_id)) = (&body.source, &body.source_id) {
        // Resolve metadata if not provided by the client
        let (title, artist, album, cover, duration) = if body.title.is_some() {
            (
                body.title.clone().unwrap_or_default(),
                body.artist_name.clone().unwrap_or_default(),
                body.album_title.clone(),
                body.cover_path.clone(),
                body.duration_ms.unwrap_or(0),
            )
        } else {
            // Fetch track metadata from the streaming service
            let registry = state.services.lock().await;
            if let Some(svc) = registry.get(source) {
                let svc = svc.lock().await;
                match svc.get_track(source_id).await {
                    Ok(t) => (
                        t.title,
                        t.artist,
                        t.album,
                        t.cover_path,
                        t.duration_ms as i64,
                    ),
                    Err(_) => ("Unknown".into(), String::new(), None, None, 0),
                }
            } else {
                ("Unknown".into(), String::new(), None, None, 0)
            }
        };

        let queue_item = vec![(
            source_id.clone(),
            title,
            artist,
            album,
            cover,
            duration,
            Some(source.clone()),
        )];
        if let Err(e) = queue_repo.append_streaming_queue(zone_id, &queue_item) {
            warn!(zone_id, error = %e, "append_streaming_queue_failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        let new_length = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let current_pos = state.playback.get_state(zone_id).await.queue_position;
        state
            .playback
            .update_queue_info(zone_id, current_pos, new_length)
            .await;
        persist_queue_async(&state, zone_id);
        return (
            StatusCode::CREATED,
            Json(json!({ "added": 1, "queue_length": new_length })),
        )
            .into_response();
    }

    // --- Local tracks ---
    if body.track_ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "track_ids or source+source_id required".to_string(),
        )
            .into_response();
    }
    match queue_repo.add_tracks(zone_id, &body.track_ids, body.position) {
        Ok(_) => {
            let new_length = queue_repo.count(zone_id).unwrap_or(0);
            let current_pos = state.playback.get_state(zone_id).await.queue_position;
            state
                .playback
                .update_queue_info(zone_id, current_pos, new_length)
                .await;
            persist_queue_async(&state, zone_id);
            (
                StatusCode::CREATED,
                Json(json!({ "added": body.track_ids.len(), "queue_length": new_length })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn queue_move(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueMoveRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let mut items = queue_repo.get_queue(zone_id).unwrap_or_default();
    let from = body.from_position as usize;
    let to = body.to_position as usize;

    if from < items.len() && to < items.len() {
        let item = items.remove(from);
        items.insert(to, item);
        let track_ids: Vec<i64> = items.iter().map(|i| i.track_id).collect();
        queue_repo.set_queue(zone_id, &track_ids).ok();
        persist_queue_async(&state, zone_id);
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
    match state
        .orchestrator
        .play_from_queue(zone_id, body.position)
        .await
    {
        Ok(result) => {
            persist_queue_async(&state, zone_id);
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn queue_clear(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    queue_repo.clear(zone_id).ok();
    state.playback.stop_and_clear(zone_id).await;
    state.playback.update_queue_info(zone_id, 0, 0).await;
    // Delete the persisted queue file
    let db_path = state.config.db_path.clone();
    tokio::task::spawn_blocking(move || {
        tune_core::queue_persistence::delete_queue_file(&db_path, zone_id);
    });
    state.event_bus.emit(
        "playback.queue.cleared",
        serde_json::json!({ "zone_id": zone_id }),
    );
    StatusCode::NO_CONTENT
}

async fn queue_remove(
    State(state): State<AppState>,
    Path((zone_id, position)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Try the local play_queue first.
    match queue_repo.remove_at(zone_id, position) {
        Ok(true) => {
            let new_length = queue_repo.count(zone_id).unwrap_or(0);
            let current_pos = state.playback.get_state(zone_id).await.queue_position;
            let adjusted_pos = if position < current_pos {
                current_pos - 1
            } else {
                current_pos
            };
            state
                .playback
                .update_queue_info(zone_id, adjusted_pos, new_length)
                .await;
            persist_queue_async(&state, zone_id);
            state.event_bus.emit(
                "playback.queue.track_removed",
                json!({ "zone_id": zone_id, "position": position }),
            );
            return Json(json!({ "queue_length": new_length })).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Ok(false) => { /* fall through to streaming_queue */ }
    }

    // Fall back to the streaming_queue (Tidal, Qobuz, Deezer, etc.).
    match queue_repo.remove_streaming_at(zone_id, position) {
        Ok(true) => {
            let new_length = queue_repo.count_streaming(zone_id).unwrap_or(0);
            let current_pos = state.playback.get_state(zone_id).await.queue_position;
            let adjusted_pos = if position < current_pos {
                current_pos - 1
            } else {
                current_pos
            };
            state
                .playback
                .update_queue_info(zone_id, adjusted_pos, new_length)
                .await;
            persist_queue_async(&state, zone_id);
            state.event_bus.emit(
                "playback.queue.track_removed",
                json!({ "zone_id": zone_id, "position": position }),
            );
            Json(json!({ "queue_length": new_length })).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "position not found in queue" })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn save_queue_as_playlist(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SaveAsPlaylistRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    if items.is_empty() {
        return (StatusCode::BAD_REQUEST, "queue is empty").into_response();
    }
    let track_ids: Vec<i64> = items.iter().map(|i| i.track_id).collect();
    let name = body
        .name
        .unwrap_or_else(|| format!("Queue - Zone {zone_id}"));
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    match playlist_repo.create(&name, None) {
        Ok(id) => {
            playlist_repo.add_tracks(id, &track_ids, None).ok();
            (
                StatusCode::CREATED,
                Json(json!({"id": id, "name": name, "track_count": track_ids.len()})),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
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

async fn get_sleep(State(_state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
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

async fn get_eq(State(_state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
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

/// Transfer current track from one zone to another (path-based, backward compat).
/// Copies the full queue + position and optionally stops the source zone.
async fn transfer_playback(
    State(state): State<AppState>,
    Path((from_zone, target_zone)): Path<(i64, i64)>,
) -> impl IntoResponse {
    do_transfer(&state, from_zone, target_zone, true).await
}

/// Transfer queue between zones via JSON body (Sergio #464).
/// POST /zones/{id}/transfer  { "target_zone_id": 2, "stop_source": true }
async fn transfer_queue(
    State(state): State<AppState>,
    Path(from_zone): Path<i64>,
    Json(body): Json<TransferRequest>,
) -> impl IntoResponse {
    do_transfer(&state, from_zone, body.target_zone_id, body.stop_source).await
}

/// Shared implementation: copy queue + now playing from source to target zone.
async fn do_transfer(
    state: &AppState,
    from_zone: i64,
    target_zone: i64,
    stop_source: bool,
) -> axum::response::Response {
    let current = state.playback.get_state(from_zone).await;
    if current.now_playing.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "nothing playing to transfer"})),
        )
            .into_response();
    }

    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Copy local queue (play_queue table)
    let local_items = queue_repo.get_queue(from_zone).unwrap_or_default();
    if !local_items.is_empty() {
        let track_ids: Vec<i64> = local_items.iter().map(|i| i.track_id).collect();
        let current_pos = local_items.iter().position(|i| i.is_current).unwrap_or(0) as i64;
        if let Err(e) = queue_repo.set_queue(target_zone, &track_ids) {
            warn!(from_zone, target_zone, error = %e, "transfer_set_queue_failed");
        } else if current_pos > 0 {
            queue_repo.set_current(target_zone, current_pos).ok();
        }
    }

    // Copy streaming queue
    let streaming_items = queue_repo
        .get_streaming_queue(from_zone)
        .unwrap_or_default();
    if !streaming_items.is_empty() {
        let tracks: Vec<(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        )> = streaming_items
            .iter()
            .map(|item| {
                (
                    item["source_id"].as_str().unwrap_or("").to_string(),
                    item["title"].as_str().unwrap_or("").to_string(),
                    item["artist_name"].as_str().unwrap_or("").to_string(),
                    item["album_title"].as_str().map(String::from),
                    item["cover_path"].as_str().map(String::from),
                    item["duration_ms"].as_i64().unwrap_or(0),
                    item["source"].as_str().map(String::from),
                )
            })
            .collect();
        if let Err(e) = queue_repo.set_streaming_queue(target_zone, &tracks) {
            warn!(from_zone, target_zone, error = %e, "transfer_streaming_queue_failed");
        }
    }

    let queue_length = if !local_items.is_empty() {
        local_items.len() as i64
    } else {
        streaming_items.len() as i64
    };

    // Transfer now-playing and playback state
    let np = current.now_playing.unwrap();
    state.playback.play(target_zone, np).await;
    state.playback.set_volume(target_zone, current.volume).await;
    state
        .playback
        .update_queue_info(target_zone, current.queue_position, queue_length)
        .await;

    // Start playback on the target device via the orchestrator if a device is assigned
    let has_output = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(target_zone)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id)
        .is_some();
    if has_output {
        if let Err(e) = state
            .orchestrator
            .play_from_queue(target_zone, current.queue_position)
            .await
        {
            warn!(target_zone, error = %e, "transfer_play_on_target_failed");
        }
    }

    if stop_source {
        state.orchestrator.stop(from_zone, None).await;
    }

    // Persist queue state for the target zone
    let target_state = state.playback.get_state(target_zone).await;
    let db_path = state.config.db_path.clone();
    let backend_clone = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        tune_core::queue_persistence::save_queue(
            &backend_clone,
            &db_path,
            target_zone,
            &target_state,
        );
    });

    state.event_bus.emit(
        "playback.transferred",
        json!({
            "from_zone": from_zone,
            "target_zone": target_zone,
            "stop_source": stop_source,
            "queue_length": queue_length,
        }),
    );

    Json(json!({
        "from_zone": from_zone,
        "target_zone": target_zone,
        "status": "transferred",
        "queue_length": queue_length,
        "stop_source": stop_source,
    }))
    .into_response()
}

async fn get_alarms(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
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
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
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
    use tune_core::db::backend::ToSqlValue;
    let days = body.days.unwrap_or_else(|| "1,2,3,4,5,6,7".into());
    let source_type = body.source_type.unwrap_or_else(|| "playlist".into());
    let volume = body.volume.unwrap_or(0.3);
    let fade_in_seconds = body.fade_in_seconds.unwrap_or(30);
    match state.backend.execute(
        "INSERT INTO alarms (zone_id, time, days, source_type, source_id, volume, fade_in_seconds) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[&zone_id as &dyn ToSqlValue, &body.time as &dyn ToSqlValue, &days as &dyn ToSqlValue, &source_type as &dyn ToSqlValue, &body.source_id as &dyn ToSqlValue, &volume as &dyn ToSqlValue, &fade_in_seconds as &dyn ToSqlValue],
    ) {
        Ok(_) => {
            let id = state.backend.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_alarm(
    State(state): State<AppState>,
    Path((_zone_id, alarm_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    state
        .db
        .execute("DELETE FROM alarms WHERE id = ?", &[&alarm_id])
        .ok();
    StatusCode::NO_CONTENT
}

fn get_zone_device_id(state: &AppState, zone_id: i64) -> Option<String> {
    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id)
}

// ---------------------------------------------------------------------------
// Zone Pins
// ---------------------------------------------------------------------------

use tune_core::db::settings_repo::SettingsRepo;

#[derive(Deserialize, serde::Serialize, Clone)]
struct ZonePin {
    index: usize,
    title: String,
    uri: String,
    #[serde(rename = "type")]
    pin_type: String,
}

fn pins_key(zone_id: i64) -> String {
    format!("zone_{zone_id}_pins")
}

fn load_pins(state: &AppState, zone_id: i64) -> Vec<ZonePin> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get(&pins_key(zone_id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_pins(state: &AppState, zone_id: i64, pins: &[ZonePin]) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            &pins_key(zone_id),
            &serde_json::to_string(pins).unwrap_or_default(),
        )
        .ok();
}

async fn get_zone_pins(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let pins = load_pins(&state, zone_id);
    Json(json!(pins))
}

async fn set_zone_pin(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<ZonePin>,
) -> impl IntoResponse {
    let mut pins = load_pins(&state, zone_id);
    // Replace at index or append
    if let Some(existing) = pins.iter_mut().find(|p| p.index == body.index) {
        *existing = body.clone();
    } else {
        pins.push(body.clone());
    }
    save_pins(&state, zone_id, &pins);
    (StatusCode::CREATED, Json(json!(body))).into_response()
}

async fn clear_zone_pin(
    State(state): State<AppState>,
    Path((zone_id, index)): Path<(i64, usize)>,
) -> impl IntoResponse {
    let mut pins = load_pins(&state, zone_id);
    pins.retain(|p| p.index != index);
    save_pins(&state, zone_id, &pins);
    StatusCode::NO_CONTENT
}

async fn invoke_zone_pin(
    State(state): State<AppState>,
    Path((zone_id, index)): Path<(i64, usize)>,
) -> impl IntoResponse {
    let pins = load_pins(&state, zone_id);
    let Some(pin) = pins.iter().find(|p| p.index == index) else {
        return (StatusCode::NOT_FOUND, "pin not found").into_response();
    };

    // Build a play request from the pin
    let output_device_id = get_zone_device_id(&state, zone_id);
    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: None,
        source: Some(pin.pin_type.clone()),
        source_id: Some(pin.uri.clone()),
        title: Some(pin.title.clone()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
    };
    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn save_queue_as_pin(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<ZonePin>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    if items.is_empty() {
        return (StatusCode::BAD_REQUEST, "queue is empty").into_response();
    }
    let mut pins = load_pins(&state, zone_id);
    let pin = ZonePin {
        index: body.index,
        title: body.title,
        uri: format!("queue:zone:{zone_id}"),
        pin_type: "queue".into(),
    };
    if let Some(existing) = pins.iter_mut().find(|p| p.index == pin.index) {
        *existing = pin.clone();
    } else {
        pins.push(pin.clone());
    }
    save_pins(&state, zone_id, &pins);
    (StatusCode::CREATED, Json(json!(pin))).into_response()
}

// ---------------------------------------------------------------------------
// Audiophile / Quality / Audio-Profile per-zone settings
// ---------------------------------------------------------------------------

async fn get_audiophile(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audiophile");
    let val = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({ "enabled": false }));
    Json(val)
}

async fn set_audiophile(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audiophile");
    settings.set(&key, &body.to_string()).ok();
    Json(body)
}

async fn get_quality(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_quality");
    let val = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({ "max_sample_rate": null, "max_bit_depth": null, "prefer_hires": true }));
    Json(val)
}

async fn set_quality(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_quality");
    settings.set(&key, &body.to_string()).ok();
    Json(body)
}

async fn share_now_playing(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let ps = state.playback.get_state(zone_id).await;
    let Some(np) = ps.now_playing else {
        return (StatusCode::BAD_REQUEST, "nothing playing").into_response();
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let token = format!("{:032x}", nanos ^ (zone_id as u128 * 0x9e3779b97f4a7c15));
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let data = json!({
        "title": np.title,
        "artist_name": np.artist_name,
        "album_title": np.album_title,
        "cover_path": np.cover_path,
        "source": np.source,
    });
    settings
        .set(&format!("share_{token}"), &data.to_string())
        .ok();
    Json(json!({
        "token": token,
        "url": format!("/shared/{token}"),
        "track": data,
    }))
    .into_response()
}

async fn get_audio_profile(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audio_profile");
    let val = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({ "name": "default" }));
    Json(val)
}

async fn set_audio_profile(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audio_profile");
    settings.set(&key, &body.to_string()).ok();
    Json(body)
}

// ---------------------------------------------------------------------------
// Shuffle All (global playback)
// ---------------------------------------------------------------------------

pub async fn shuffle_all(State(state): State<AppState>) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    let all_ids = track_repo.random_ids(100).unwrap_or_default();
    if all_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "library is empty").into_response();
    }

    let zone_id = 1i64; // default zone
    queue_repo.set_queue(zone_id, &all_ids).ok();

    let first_id = all_ids[0];
    let track = track_repo.get(first_id).ok().flatten();
    let output_device_id = get_zone_device_id(&state, zone_id);

    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: Some(first_id),
        source: None,
        source_id: None,
        title: track.as_ref().map(|t| t.title.clone()),
        artist_name: track.as_ref().and_then(|t| t.artist_name.clone()),
        album_title: track.as_ref().and_then(|t| t.album_title.clone()),
        cover_url: track.as_ref().and_then(|t| t.cover_path.clone()),
        duration_ms: track.as_ref().map(|t| t.duration_ms),
    };
    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            state
                .playback
                .update_queue_info(zone_id, 0, all_ids.len() as i64)
                .await;
            let mut resp = json!({ "zone_id": zone_id, "tracks_queued": all_ids.len(), "output_sent": result.output_sent });
            if let Some(ref err) = result.error {
                resp.as_object_mut()
                    .unwrap()
                    .insert("error".into(), json!(err));
            }
            Json(resp).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
