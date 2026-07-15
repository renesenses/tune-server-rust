use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::orchestrator::PlayResult;

use crate::error::AppError;
use crate::state::AppState;

/// Map an orchestrator play error to an appropriate HTTP status code.
///
/// Streaming service failures (yt-dlp, API errors, auth) are upstream issues
/// and should be 502 Bad Gateway, not 500 Internal Server Error.
/// Device-offline errors are 503 Service Unavailable.
/// Everything else is 500.
fn play_error_response(e: String) -> (StatusCode, String) {
    let code = if e.contains("YouTube")
        || e.contains("youtube")
        || e.contains("yt-dlp")
        || e.contains("yt_dlp")
        || e.contains("stream url")
        || e.contains("Streaming service")
        || e.contains("streaming")
        || e.contains("Qobuz")
        || e.contains("qobuz")
        || e.contains("Tidal")
        || e.contains("tidal")
        || e.contains("Deezer")
        || e.contains("deezer")
        || e.contains("Spotify")
        || e.contains("spotify")
        || e.contains("401")
        || e.contains("403")
        || e.contains("not playable")
        || e.contains("extraction")
    {
        StatusCode::BAD_GATEWAY
    } else if e.contains("offline") || e.contains("Output device") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (code, e)
}

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
            "format": np.format,
            "sample_rate": np.sample_rate,
            "bit_depth": np.bit_depth,
            "genre": np.genre,
            "year": np.year,
        })),
        "position_ms": zone_state.position_ms,
        "queue_length": zone_state.queue_length,
        "queue_position": zone_state.queue_position,
        "muted": zone_state.muted,
    });
    // Include stream_url ONLY for browser playback zones, so the web client can
    // feed it to an HTML5 <audio> element. For a network output (DLNA / Chromecast
    // / AirPlay / SlimProto / local), an open web-client tab that fetched this URL
    // would consume the SAME single-consumer stream (streamer.rs mpsc) as the
    // renderer and starve it — playback stalled or skipped after a few tracks
    // until the tab was closed (forum: eric, #954; matches "close the tab and the
    // sound comes back").
    let is_browser_zone =
        zone_db.as_ref().and_then(|z| z.output_type.as_deref()) == Some("browser");
    if is_browser_zone {
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
    }
    // Include signal_path (the bit-perfect indicator) so the play / next /
    // previous / resume responses carry it, matching GET /zones/{id}. Without
    // it the indicator was absent on the FIRST track — playAndSync renders this
    // play response — and only appeared from the SECOND track on, because
    // nextAndSync refreshes via GET /zones/{id}, which does include it
    // (forum #1012, Bilou).
    if let Some(ref zone) = zone_db {
        let devices = state.scanner.lock().await.devices().await;
        let renderer_label = zone
            .output_device_id
            .as_deref()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
        #[cfg(feature = "local-audio")]
        let audio_backend =
            tune_core::outputs::local::active_backend_name(&state.config.local_audio_backend);
        #[cfg(not(feature = "local-audio"))]
        let audio_backend = "none";
        let signal_path = crate::routes::zones::build_signal_path_pub(
            &zone_state,
            zone,
            &state.backend,
            renderer_label,
            audio_backend,
        );
        v.as_object_mut()
            .unwrap()
            .insert("signal_path".into(), json!(signal_path));
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
    seek_ms: Option<u64>,
    temp_file_path: Option<String>,
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
    track_id: Option<i64>,
    position: Option<i64>,
    // Streaming track fields (single)
    source: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
    // Batch streaming tracks: [{source, source_id, title?, artist_name?, ...}]
    #[serde(default)]
    tracks: Vec<StreamingTrackItem>,
}

#[derive(Deserialize)]
struct StreamingTrackItem {
    source: String,
    source_id: String,
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
        .route("/upload", post(upload_audio_file))
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
        // DSP route is in zones.rs (/{id}/dsp GET+PUT)
        .route("/{id}/crossfade", get(get_crossfade).post(set_crossfade))
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

/// Replace a zone's queue, retrying briefly on the transient
/// "cannot start a transaction within a transaction" error.
///
/// A library scan holds a per-batch write transaction on the shared SQLite
/// connection (BEGIN IMMEDIATE … COMMIT) while releasing the connection mutex
/// between statements, so a concurrent `set_queue` sees an open transaction and
/// fails. Each scan batch commits within ~1–2 s, so a few short async waits let
/// playback replace the queue instead of failing silently and leaving the user
/// stuck on the current track (Yves: impossible de quitter le dernier MP3
/// pendant qu'un scan tourne). Non-transient errors return immediately.
async fn set_queue_retrying(
    queue_repo: &PlayQueueRepo,
    zone_id: i64,
    track_ids: &[i64],
) -> Result<(), String> {
    const MAX_ATTEMPTS: usize = 12;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        match queue_repo.set_queue(zone_id, track_ids) {
            Ok(()) => return Ok(()),
            Err(e) if e.contains("within a transaction") => {
                last_err = e;
                if attempt + 1 < MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err)
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
                    seek_ms: None,
                    temp_file_path: None,
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        // Restore queue_length from DB so the poller can
                        // advance tracks (fixes repeat-all after restart).
                        let qr = PlayQueueRepo::with_backend(state.backend.clone());
                        let local_c = qr.count(zone_id).unwrap_or(0);
                        let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                        // Combined length: a zone can hold a local queue AND a
                        // streaming queue at once. Sum them so queue-end detection
                        // doesn't drop the streaming tail → silence (Sandro S1b).
                        let q_len = local_c + stream_c;
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
                    Err(e) => {
                        tracing::warn!(zone_id, error = %e, "play_resume_failed_trying_queue");
                        // Fallback: try to play from queue position 0
                        let qr = PlayQueueRepo::with_backend(state.backend.clone());
                        let local_c = qr.count(zone_id).unwrap_or(0);
                        let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                        // Combined length: a zone can hold a local queue AND a
                        // streaming queue at once. Sum them so queue-end detection
                        // doesn't drop the streaming tail → silence (Sandro S1b).
                        let q_len = local_c + stream_c;
                        if q_len > 0 {
                            let pos = current.queue_position.min(q_len - 1);
                            state.playback.update_queue_info(zone_id, pos, q_len).await;
                            if let Ok(result) =
                                state.orchestrator.play_from_queue(zone_id, pos).await
                            {
                                return Json(
                                    build_zone_json_with_result(&state, zone_id, &result).await,
                                )
                                .into_response();
                            }
                        }
                        (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
                    }
                };
            }
            // No now_playing — try queue fallback
            {
                let qr = PlayQueueRepo::with_backend(state.backend.clone());
                let local_c = qr.count(zone_id).unwrap_or(0);
                let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                // Combined length: a zone can hold a local queue AND a
                // streaming queue at once. Sum them so queue-end detection
                // doesn't drop the streaming tail → silence (Sandro S1b).
                let q_len = local_c + stream_c;
                if q_len > 0 {
                    let current = state.playback.get_state(zone_id).await;
                    let pos = current.queue_position.min(q_len - 1);
                    state.playback.update_queue_info(zone_id, pos, q_len).await;
                    if let Ok(result) = state.orchestrator.play_from_queue(zone_id, pos).await {
                        return Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response();
                    }
                }
            }
            // Last resort: resume from last_track saved in DB (after stop)
            {
                let zone_repo =
                    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
                if let Ok(Some(zone)) = zone_repo.get(zone_id) {
                    if let Some(track_id) = zone.last_track_id {
                        let output_device_id = get_zone_device_id(&state, zone_id);
                        let orch_req = tune_core::orchestrator::PlayRequest {
                            zone_id,
                            output_device_id,
                            track_id: Some(track_id),
                            source: zone.last_track_source.clone().filter(|s| s != "local"),
                            source_id: zone.last_track_source_id.clone(),
                            title: None,
                            artist_name: None,
                            album_title: None,
                            cover_url: None,
                            duration_ms: None,
                            seek_ms: None,
                            temp_file_path: None,
                        };
                        if let Ok(result) = state.orchestrator.play(orch_req).await {
                            return Json(
                                build_zone_json_with_result(&state, zone_id, &result).await,
                            )
                            .into_response();
                        }
                    }
                }
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

        // Write queue BEFORE play so WS-triggered fetchQueue() finds it
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
            seek_ms: None,
            temp_file_path: None,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
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

        // Write queue BEFORE play so WS-triggered fetchQueue() finds it
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
            seek_ms: None,
            temp_file_path: None,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
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
            seek_ms: None,
            temp_file_path: None,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                // If this track is already part of the loaded streaming queue
                // (e.g. the user pressed Stop then Play again on a track from an
                // album/playlist that is already queued), keep the full queue and
                // just move the current position onto it. Replacing it with a
                // single-track queue would truncate the album down to the current
                // title (Pierre M: "Si STOP et relance, la file d'attente se
                // limite au titre en cours").
                let existing = queue_repo.get_streaming_queue(zone_id).unwrap_or_default();
                let existing_idx = existing.iter().position(|it| {
                    it["source_id"].as_str() == Some(source_id_val.as_str())
                        && (source_for_q.is_none()
                            || it["source"].as_str() == source_for_q.as_deref())
                });
                if let Some(idx) = existing_idx {
                    state
                        .playback
                        .update_queue_info(zone_id, idx as i64, existing.len() as i64)
                        .await;
                } else {
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
                }
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
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
                seek_ms: None,
                temp_file_path: None,
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
        // No now_playing — try queue fallback (same as empty-body path)
        let qr_fallback = PlayQueueRepo::with_backend(state.backend.clone());
        let local_c = qr_fallback.count(zone_id).unwrap_or(0);
        let stream_c = qr_fallback.count_streaming(zone_id).unwrap_or(0);
        // Combined length: a zone can hold a local queue AND a
        // streaming queue at once. Sum them so queue-end detection
        // doesn't drop the streaming tail → silence (Sandro S1b).
        let q_len = local_c + stream_c;
        if q_len > 0 {
            let current = state.playback.get_state(zone_id).await;
            let pos = current.queue_position.min(q_len - 1);
            state.playback.update_queue_info(zone_id, pos, q_len).await;
            if let Ok(result) = state.orchestrator.play_from_queue(zone_id, pos).await {
                persist_queue_async(&state, zone_id);
                return Json(build_zone_json_with_result(&state, zone_id, &result).await)
                    .into_response();
            }
        }
        return (StatusCode::BAD_REQUEST, "no track source specified").into_response();
    };

    if track_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to play").into_response();
    }

    if let Err(e) = set_queue_retrying(&queue_repo, zone_id, &track_ids).await {
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
        seek_ms: body.seek_ms,
        temp_file_path: body.temp_file_path,
    };

    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            let qr = PlayQueueRepo::with_backend(state.backend.clone());
            let local_c = qr.count(zone_id).unwrap_or(0);
            let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
            // Combined length: a zone can hold a local queue AND a
            // streaming queue at once. Sum them so queue-end detection
            // doesn't drop the streaming tail → silence (Sandro S1b).
            let q_len = local_c + stream_c;
            let q_len = if q_len > 0 {
                q_len
            } else {
                track_ids.len() as i64
            };
            state
                .playback
                .update_queue_info(zone_id, start, q_len)
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
                seek_ms: None,
                temp_file_path: None,
            };
            return match state.orchestrator.play(orch_req).await {
                Ok(result) => {
                    // Restore queue_length from DB so the poller can
                    // advance tracks (fixes repeat-all after restart).
                    let qr = PlayQueueRepo::with_backend(state.backend.clone());
                    let local_c = qr.count(zone_id).unwrap_or(0);
                    let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
                    // Combined length: a zone can hold a local queue AND a
                    // streaming queue at once. Sum them so queue-end detection
                    // doesn't drop the streaming tail → silence (Sandro S1b).
                    let q_len = local_c + stream_c;
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

    // Stopped with no now_playing (e.g. after server restart) — try to
    // play the first track from the queue instead of a bare resume.
    if current.state == tune_core::playback::PlayState::Stopped {
        let qr = PlayQueueRepo::with_backend(state.backend.clone());
        let output_device_id = get_zone_device_id(&state, zone_id);
        // Try streaming queue first, then local queue
        let streaming_items = qr.get_streaming_queue(zone_id).unwrap_or_default();
        if let Some(first) = streaming_items.first() {
            let source = first
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source_id = first
                .get("source_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = first
                .get("title")
                .and_then(|v| v.as_str())
                .map(String::from);
            if !source.is_empty() && !source_id.is_empty() {
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: None,
                    source: Some(source),
                    source_id: Some(source_id),
                    title,
                    artist_name: first
                        .get("artist_name")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    album_title: first
                        .get("album_title")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    cover_url: first
                        .get("cover_path")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    duration_ms: first.get("duration_ms").and_then(|v| v.as_i64()),
                    ..Default::default()
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        let q_len = qr.count_streaming(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            state.playback.update_queue_info(zone_id, 0, q_len).await;
                        }
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                };
            }
        }
        let local_items = qr.get_queue(zone_id).unwrap_or_default();
        if let Some(first) = local_items.first() {
            {
                let track_id = first.track_id;
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: Some(track_id),
                    source: None,
                    source_id: None,
                    title: None,
                    artist_name: None,
                    album_title: None,
                    cover_url: None,
                    duration_ms: None,
                    seek_ms: None,
                    temp_file_path: None,
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        let q_len = qr.count(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            state.playback.update_queue_info(zone_id, 0, q_len).await;
                        }
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                };
            }
        }
        // Nothing in the queue — return stopped state, don't set Playing
        return Json(build_zone_json(&state, zone_id).await).into_response();
    }

    // For a normal resume (paused → playing), also ensure queue_length is
    // populated — it may be zero after a server restart.
    {
        let qr = PlayQueueRepo::with_backend(state.backend.clone());
        let local_c = qr.count(zone_id).unwrap_or(0);
        let stream_c = qr.count_streaming(zone_id).unwrap_or(0);
        // Combined length: a zone can hold a local queue AND a
        // streaming queue at once. Sum them so queue-end detection
        // doesn't drop the streaming tail → silence (Sandro S1b).
        let q_len = local_c + stream_c;
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
    info!(zone_id = zone_id, "api_next_requested");
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
    info!(zone_id = zone_id, "api_previous_requested");
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
    let vol_int = (body.volume * 100.0).round() as i32;
    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .update_volume(zone_id, vol_int)
        .ok();
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
    let ps = state.playback.get_state(zone_id).await;

    // A zone can hold BOTH a local play_queue and a streaming_queue at once
    // (a Qobuz track added while a local file plays). Return ONE combined list —
    // local first (positions 0..L), then streaming (L..L+S) — matching the
    // combined position space the poller and orchestrator advance through.
    // The old either/or logic hid the streaming rows when a local queue was
    // present (Progman: an added Qobuz track was invisible and never played).
    // Playing a streaming album normally clears the local table via
    // set_streaming_queue, so usually exactly one is populated; merging is safe
    // and only surfaces genuine mixed queues.
    let local = queue_repo.get_queue(zone_id).unwrap_or_default();
    let local_count = local.len();
    let streaming = queue_repo.get_streaming_queue(zone_id).unwrap_or_default();

    let mut tracks: Vec<Value> = Vec::with_capacity(local_count + streaming.len());
    for it in &local {
        tracks.push(serde_json::to_value(it).unwrap_or(Value::Null));
    }
    for (i, it) in streaming.iter().enumerate() {
        let mut v = it.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("position".into(), json!(local_count + i));
        }
        tracks.push(v);
    }

    // Position: the current local item if one is marked current, otherwise the
    // playback state's combined queue position (a streaming item is current).
    let position = local
        .iter()
        .position(|i| i.is_current)
        .map(|p| p as i64)
        .unwrap_or(ps.queue_position);
    let length = tracks.len();
    Json(json!({ "tracks": tracks, "position": position, "length": length }))
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
        let local_count = queue_repo.count(zone_id).unwrap_or(0);
        let streaming_count = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let total = local_count + streaming_count;
        let current_pos = state.playback.get_state(zone_id).await.queue_position;
        state
            .playback
            .update_queue_info(zone_id, current_pos, total)
            .await;
        persist_queue_async(&state, zone_id);
        let total = queue_repo.count_streaming(zone_id).unwrap_or(0);
        return (
            StatusCode::CREATED,
            Json(json!({ "added": 1, "queue_length": total })),
        )
            .into_response();
    }

    // --- Batch streaming tracks: [{source, source_id, ...}] ---
    if !body.tracks.is_empty() {
        let mut queue_items = Vec::with_capacity(body.tracks.len());
        for item in &body.tracks {
            let (title, artist, album, cover, duration) = if item.title.is_some() {
                (
                    item.title.clone().unwrap_or_default(),
                    item.artist_name.clone().unwrap_or_default(),
                    item.album_title.clone(),
                    item.cover_path.clone(),
                    item.duration_ms.unwrap_or(0),
                )
            } else {
                let registry = state.services.lock().await;
                if let Some(svc) = registry.get(&item.source) {
                    let svc = svc.lock().await;
                    match svc.get_track(&item.source_id).await {
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
            queue_items.push((
                item.source_id.clone(),
                title,
                artist,
                album,
                cover,
                duration,
                Some(item.source.clone()),
            ));
        }
        let count = queue_items.len();
        if let Err(e) = queue_repo.append_streaming_queue(zone_id, &queue_items) {
            warn!(zone_id, error = %e, "batch_append_streaming_queue_failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        let local_count = queue_repo.count(zone_id).unwrap_or(0);
        let streaming_count = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let total = local_count + streaming_count;
        let current_pos = state.playback.get_state(zone_id).await.queue_position;
        state
            .playback
            .update_queue_info(zone_id, current_pos, total)
            .await;
        persist_queue_async(&state, zone_id);
        state.event_bus.emit(
            "playback.queue.track_added",
            json!({ "zone_id": zone_id, "added": count, "queue_length": total }),
        );
        return (
            StatusCode::CREATED,
            Json(json!({ "added": count, "queue_length": total })),
        )
            .into_response();
    }

    // --- Local tracks ---
    let mut ids = body.track_ids;
    if let Some(single) = body.track_id {
        ids.push(single);
    }
    if ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "track_ids, track_id, source+source_id, or tracks[] required".to_string(),
        )
            .into_response();
    }
    match queue_repo.add_tracks(zone_id, &ids, body.position) {
        Ok(_) => {
            let new_length = queue_repo.count(zone_id).unwrap_or(0);
            let current_pos = state.playback.get_state(zone_id).await.queue_position;
            state
                .playback
                .update_queue_info(zone_id, current_pos, new_length)
                .await;
            persist_queue_async(&state, zone_id);
            state.event_bus.emit(
                "playback.queue.track_added",
                json!({ "zone_id": zone_id, "added": ids.len(), "queue_length": new_length }),
            );
            (
                StatusCode::CREATED,
                Json(json!({ "added": ids.len(), "queue_length": new_length })),
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
    // Queue order changed — invalidate prefetched track
    state.orchestrator.clear_prefetch().await;
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
        state.event_bus.emit(
            "playback.queue.moved",
            json!({ "zone_id": zone_id, "from": from, "to": to }),
        );
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
    state.orchestrator.clear_prefetch().await;
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
    // Queue shape changed — invalidate prefetched track
    state.orchestrator.clear_prefetch().await;
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Try the local play_queue first.
    match queue_repo.remove_at(zone_id, position) {
        Ok(true) => {
            let new_length = queue_repo.count(zone_id).unwrap_or(0)
                + queue_repo.count_streaming(zone_id).unwrap_or(0);
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
            let new_length = queue_repo.count(zone_id).unwrap_or(0)
                + queue_repo.count_streaming(zone_id).unwrap_or(0);
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

/// Per-zone sleep-timer remaining seconds. Counts down only while the zone is
/// actually playing (pause-aware), so a paused zone doesn't burn its timer.
/// A single ticker task per zone owns the countdown and stops playback at 0.
static SLEEP_TIMERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

async fn set_sleep(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SleepRequest>,
) -> Json<Value> {
    if body.minutes == 0 {
        SLEEP_TIMERS.lock().unwrap().remove(&zone_id);
        return Json(json!({ "sleep_timer": null, "zone_id": zone_id }));
    }

    let remaining = body.minutes * 60;
    // Insert/refresh the remaining seconds. `starting` is true only when no
    // ticker is currently running for this zone, so we never spawn duplicates.
    let starting = {
        let mut timers = SLEEP_TIMERS.lock().unwrap();
        let existed = timers.contains_key(&zone_id);
        timers.insert(zone_id, remaining);
        !existed
    };

    if starting {
        let playback = state.playback.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let playing = playback.get_state(zone_id).await.state
                    == tune_core::playback::PlayState::Playing;
                let left = {
                    let mut timers = SLEEP_TIMERS.lock().unwrap();
                    match timers.get_mut(&zone_id) {
                        None => break, // cancelled
                        Some(secs) => {
                            if playing && *secs > 0 {
                                *secs -= 1;
                            }
                            *secs
                        }
                    }
                };
                if left == 0 {
                    playback.stop(zone_id).await;
                    SLEEP_TIMERS.lock().unwrap().remove(&zone_id);
                    break;
                }
            }
        });
    }

    Json(json!({
        "sleep_timer": { "minutes": body.minutes, "zone_id": zone_id },
    }))
}

async fn get_sleep(State(_state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let remaining = SLEEP_TIMERS.lock().unwrap().get(&zone_id).copied();
    Json(json!({
        "zone_id": zone_id,
        "active": remaining.is_some(),
        "remaining_seconds": remaining,
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
struct CrossfadeSettings {
    enabled: bool,
    duration: Option<f64>,
}

/// Read the persisted crossfade settings for a zone.
///
/// NOTE: crossfade is not yet applied by the playback engine (the
/// `CrossfadeHandler` in tune-core is not wired into the transition path).
/// This endpoint only persists/returns the user's preference so the UI can
/// round-trip it without a 405 — actually applying the fade is a follow-up.
async fn get_crossfade(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get(&format!("crossfade_enabled:{zone_id}"))
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let duration = settings
        .get(&format!("crossfade_duration:{zone_id}"))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(3.0);
    Json(json!({
        "enabled": enabled,
        "duration": duration,
    }))
}

async fn set_crossfade(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<CrossfadeSettings>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let duration = body.duration.unwrap_or(3.0);
    let _ = settings.set(
        &format!("crossfade_enabled:{zone_id}"),
        &body.enabled.to_string(),
    );
    let _ = settings.set(
        &format!("crossfade_duration:{zone_id}"),
        &duration.to_string(),
    );
    Json(json!({
        "zone_id": zone_id,
        "crossfade_enabled": body.enabled,
        "crossfade_duration": duration,
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
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let sql = format!(
        "SELECT id, zone_id, time, enabled, days, source_type, source_id, volume, fade_in_seconds \
         FROM alarms WHERE zone_id = {p1} ORDER BY time"
    );
    let rows = state
        .backend
        .query_many(&sql, &[&zone_id as &dyn ToSqlValue])
        .unwrap_or_default();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "zone_id": r.get(1).and_then(|v| v.as_i64()),
                "time": r.get(2).and_then(|v| v.as_string()),
                "enabled": r.get(3).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "days": r.get(4).and_then(|v| v.as_string()),
                "source_type": r.get(5).and_then(|v| v.as_string()),
                "source_id": r.get(6).and_then(|v| v.as_i64()),
                "volume": r.get(7).and_then(|v| v.as_f64()),
                "fade_in_seconds": r.get(8).and_then(|v| v.as_i64()),
            })
        })
        .collect();
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
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    state
        .backend
        .execute(
            &format!("DELETE FROM alarms WHERE id = {p1}"),
            &[&alarm_id as &dyn ToSqlValue],
        )
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
        seek_ms: None,
        temp_file_path: None,
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

#[derive(serde::Deserialize)]
pub struct ShuffleAllQuery {
    zone_id: Option<i64>,
    search_query: Option<String>,
    genre: Option<String>,
    album_id: Option<i64>,
    artist_id: Option<i64>,
}

pub async fn shuffle_all(
    State(state): State<AppState>,
    Query(q): Query<ShuffleAllQuery>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Honor the current library filter context so the shuffle applies to the
    // visible results, not the whole library, and target the caller's zone
    // (Sergio: shuffle from a search result did nothing / played nowhere).
    let mut all_ids: Vec<i64> = if let Some(aid) = q.album_id {
        track_repo
            .list_by_album(aid)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default()
    } else if let Some(arid) = q.artist_id {
        track_repo
            .list_by_artist(arid)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default()
    } else if let Some(sq) = q
        .search_query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        track_repo
            .search(sq, 500)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default()
    } else if let Some(g) = q.genre.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        track_repo
            .search(g, 500)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default()
    } else {
        // Whole-library shuffle: queue every track, not just 100 (tester wanted
        // to shuffle the entire library). Album/artist shuffles above are
        // already uncapped, so this makes the no-filter case consistent. IDs are
        // i64s so even a large library is cheap to hold; the Fisher-Yates below
        // reshuffles them anyway.
        track_repo.random_ids(i64::MAX).unwrap_or_default()
    };
    if all_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to shuffle").into_response();
    }

    // Fisher-Yates shuffle (xorshift64, time-seeded — no rand dependency).
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..all_ids.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        all_ids.swap(i, j);
    }

    let zone_id = q.zone_id.unwrap_or(1);
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
        seek_ms: None,
        temp_file_path: None,
    };
    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            state
                .playback
                .update_queue_info(zone_id, 0, all_ids.len() as i64)
                .await;
            let mut resp = json!({ "zone_id": zone_id, "track_count": all_ids.len(), "tracks_queued": all_ids.len(), "output_sent": result.output_sent });
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

async fn upload_audio_file(mut multipart: axum::extract::Multipart) -> impl IntoResponse {
    let upload_dir = std::path::Path::new("/tmp/tune-upload");
    let _ = std::fs::create_dir_all(upload_dir);
    let file_id = uuid::Uuid::new_v4().to_string();

    let mut file_data: Option<Vec<u8>> = None;
    let mut original_name = String::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "audio" {
            original_name = field.file_name().unwrap_or("unknown.wav").to_string();
            match field.bytes().await {
                Ok(bytes) => file_data = Some(bytes.to_vec()),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("upload read failed: {e}")})),
                    )
                        .into_response();
                }
            }
        }
    }

    let Some(data) = file_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no audio file in upload"})),
        )
            .into_response();
    };

    let ext = std::path::Path::new(&original_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();
    let file_path = upload_dir.join(format!("{file_id}.{ext}"));
    if let Err(e) = std::fs::write(&file_path, &data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("save failed: {e}")})),
        )
            .into_response();
    }

    let meta = tune_core::metadata::try_read_metadata(&file_path);
    let title = meta
        .as_ref()
        .ok()
        .and_then(|m| m.title.clone())
        .unwrap_or_else(|| {
            std::path::Path::new(&original_name)
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string()
        });

    (
        StatusCode::OK,
        Json(json!({
            "file_id": file_id,
            "file_path": file_path.to_string_lossy(),
            "title": title,
            "artist": meta.as_ref().ok().and_then(|m| m.artist.clone()),
            "album": meta.as_ref().ok().and_then(|m| m.album.clone()),
            "duration_ms": meta.as_ref().ok().and_then(|m| m.duration_ms).unwrap_or(0),
            "format": ext,
            "sample_rate": meta.as_ref().ok().and_then(|m| m.sample_rate),
            "bit_depth": meta.as_ref().ok().and_then(|m| m.bit_depth),
        })),
    )
        .into_response()
}
