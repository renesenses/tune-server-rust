use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::playback::NowPlaying;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct CreateRadio {
    name: String,
    #[serde(alias = "stream_url")]
    url: String,
    #[serde(alias = "homepage_url")]
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
        .route(
            "/{id}",
            get(get_radio).put(update_radio).delete(delete_radio),
        )
        .route("/{id}/favorite", post(toggle_favorite))
        .route("/{id}/play/{zone_id}", post(play_radio))
        .route("/{id}/artwork", post(set_radio_artwork))
        .route("/export.m3u", get(export_radios_m3u))
        .route("/import", post(import_radios))
}

async fn list_radios(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::new(state.db);
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn get_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
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

async fn update_radio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateRadio>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let Some(mut station) = repo.get(id).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    station.name = body.name;
    station.url = body.url;
    station.homepage = body.homepage;
    station.logo_url = body.logo_url;
    station.country = body.country;
    station.language = body.language;
    station.genre = body.genre;
    station.codec = body.codec;
    station.bitrate = body.bitrate;
    match repo.update(&station) {
        Ok(()) => Json(json!(station)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
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

    let (output_sent, output_error) = if let Some(ref did) = device_id {
        let output_arc = {
            let outputs = state.outputs.lock().await;
            outputs.get(did)
        };
        if let Some(output_arc) = output_arc {
            let output = output_arc.lock().await;
            let media = tune_core::outputs::PlayMedia {
                url: &radio.url,
                mime_type: "audio/aac",
                title: Some(&radio.name),
                cover_url: radio.logo_url.as_deref(),
                ..Default::default()
            };
            match output.play_media(&media).await {
                Ok(()) => (true, None),
                Err(e) => (false, Some(format!("Output device error: {e}"))),
            }
        } else {
            (
                false,
                Some("Device not yet discovered. Please retry in a few seconds.".into()),
            )
        }
    } else {
        (false, None)
    };

    repo.record_play(id).ok();

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({
        "zone_id": zone_id,
        "radio": radio.name,
        "output_sent": output_sent,
        "error": output_error,
        "state": zone_state,
    }))
    .into_response()
}

async fn search_radios(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Value> {
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

// ---------------------------------------------------------------------------
// Radio artwork / export / import
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SetArtworkBody {
    logo_url: String,
}

async fn set_radio_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetArtworkBody>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let Some(mut radio) = repo.get(id).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    radio.logo_url = Some(body.logo_url.clone());
    repo.update(&radio).ok();
    Json(json!({ "id": id, "logo_url": body.logo_url })).into_response()
}

async fn export_radios_m3u(State(state): State<AppState>) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let stations = repo.list().unwrap_or_default();

    let mut m3u = String::from("#EXTM3U\n");
    for s in &stations {
        m3u.push_str(&format!("#EXTINF:-1,{}\n{}\n", s.name, s.url));
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_static("audio/x-mpegurl; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_static("attachment; filename=\"radios.m3u\""),
    );
    (axum::http::StatusCode::OK, headers, m3u).into_response()
}

#[derive(Deserialize)]
struct ImportRadiosBody {
    stations: Vec<CreateRadio>,
}

async fn import_radios(
    State(state): State<AppState>,
    Json(body): Json<ImportRadiosBody>,
) -> impl IntoResponse {
    let repo = RadioRepo::new(state.db);
    let mut imported = 0i64;
    for s in &body.stations {
        let station = RadioStation {
            id: None,
            name: s.name.clone(),
            url: s.url.clone(),
            homepage: s.homepage.clone(),
            logo_url: s.logo_url.clone(),
            country: s.country.clone(),
            language: s.language.clone(),
            genre: s.genre.clone(),
            codec: s.codec.clone(),
            bitrate: s.bitrate,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        if repo.create(&station).is_ok() {
            imported += 1;
        }
    }
    (StatusCode::CREATED, Json(json!({ "imported": imported }))).into_response()
}

// ---------------------------------------------------------------------------
// Radio Favorites
// ---------------------------------------------------------------------------

pub fn radio_favorites_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_radio_favorites).post(save_radio_favorite))
        .route("/count", get(radio_favorites_count))
        .route("/is-favorite", get(is_radio_favorite))
        .route("/save-current", post(save_current_as_favorite))
        .route("/create-playlist", post(create_playlist_from_favorites))
        .route("/{fav_id}", axum::routing::delete(delete_radio_favorite))
}

#[derive(Deserialize)]
struct RadioFavPagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_radio_favorites(
    State(state): State<AppState>,
    Query(q): Query<RadioFavPagination>,
) -> Result<Json<Value>, AppError> {
    let limit = q.limit.unwrap_or(100);
    let offset = q.offset.unwrap_or(0);
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare("SELECT id, title, artist, station_name, cover_url, stream_url, saved_at FROM radio_favorites ORDER BY saved_at DESC LIMIT ? OFFSET ?")
        .and_then(|mut stmt| {
            stmt.query_map(params![limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist": row.get::<_, Option<String>>(2).ok().flatten(),
                    "station_name": row.get::<_, Option<String>>(3).ok().flatten(),
                    "cover_url": row.get::<_, Option<String>>(4).ok().flatten(),
                    "stream_url": row.get::<_, Option<String>>(5).ok().flatten(),
                    "saved_at": row.get::<_, Option<String>>(6).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

async fn radio_favorites_count(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM radio_favorites", [], |row| row.get(0))
        .unwrap_or(0);
    drop(conn);
    Ok(Json(json!({ "count": count })))
}

#[derive(Deserialize)]
struct IsFavoriteQuery {
    title: String,
    artist: Option<String>,
}

async fn is_radio_favorite(
    State(state): State<AppState>,
    Query(q): Query<IsFavoriteQuery>,
) -> Result<Json<Value>, AppError> {
    let artist = q.artist.unwrap_or_default();
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM radio_favorites WHERE title = ? AND artist = ?)",
            params![q.title, artist],
            |row| row.get(0),
        )
        .unwrap_or(false);
    drop(conn);
    Ok(Json(json!({ "is_favorite": exists })))
}

#[derive(Deserialize)]
struct SaveRadioFavorite {
    title: String,
    artist: Option<String>,
    station_name: Option<String>,
    cover_url: Option<String>,
    stream_url: Option<String>,
}

async fn save_radio_favorite(
    State(state): State<AppState>,
    Json(body): Json<SaveRadioFavorite>,
) -> impl IntoResponse {
    let artist = body.artist.unwrap_or_default();
    match state.db.execute(
        "INSERT OR IGNORE INTO radio_favorites (title, artist, station_name, cover_url, stream_url) VALUES (?, ?, ?, ?, ?)",
        &[
            &body.title as &dyn rusqlite::types::ToSql,
            &artist,
            &body.station_name.unwrap_or_default(),
            &body.cover_url,
            &body.stream_url,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio_favorite(
    State(state): State<AppState>,
    Path(fav_id): Path<i64>,
) -> impl IntoResponse {
    state
        .db
        .execute("DELETE FROM radio_favorites WHERE id = ?", &[&fav_id])
        .ok();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct SaveCurrentBody {
    zone_id: i64,
}

async fn save_current_as_favorite(
    State(state): State<AppState>,
    Json(body): Json<SaveCurrentBody>,
) -> impl IntoResponse {
    let zone_state = state.playback.get_state(body.zone_id).await;
    let Some(np) = zone_state.now_playing else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nothing playing" })),
        )
            .into_response();
    };

    let title = np.title.clone();
    let artist = np.artist_name.clone().unwrap_or_default();
    let station_name = if np.source == "radio" {
        np.album_title.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let cover_url = np.cover_path.clone();

    match state.db.execute(
        "INSERT OR IGNORE INTO radio_favorites (title, artist, station_name, cover_url) VALUES (?, ?, ?, ?)",
        &[
            &title as &dyn rusqlite::types::ToSql,
            &artist,
            &station_name,
            &cover_url,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id, "title": title, "artist": artist }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Create playlist from radio favorites
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreatePlaylistFromFavBody {
    name: Option<String>,
    playlist_name: Option<String>,
    #[allow(dead_code)]
    service: Option<String>, // accepted for forward-compat; not used yet
    limit: Option<usize>,
}

async fn create_playlist_from_favorites(
    State(state): State<AppState>,
    body: Option<Json<CreatePlaylistFromFavBody>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let favorites: Vec<(String, String)> = conn
        .prepare("SELECT title, artist FROM radio_favorites ORDER BY saved_at DESC")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    if favorites.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no favorites to create playlist from"})),
        )
            .into_response());
    }

    let (name, limit) = match body {
        Some(Json(ref b)) => {
            let n = b
                .playlist_name
                .clone()
                .or(b.name.clone())
                .unwrap_or_else(|| "Radio Favorites".into());
            let l = b.limit.unwrap_or(200);
            (n, l)
        }
        None => ("Radio Favorites".into(), 200),
    };

    let favorites: Vec<(String, String)> = if limit < favorites.len() {
        favorites.into_iter().take(limit).collect()
    } else {
        favorites
    };

    let repo = tune_core::db::playlist_repo::PlaylistRepo::new(state.db.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::new(state.db);
    let playlist_id = match repo.create(&name, None) {
        Ok(id) => id,
        Err(e) => {
            return Ok(
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
            );
        }
    };

    let mut matched = 0i64;
    for (title, artist) in &favorites {
        let q = if artist.is_empty() {
            title.clone()
        } else {
            format!("{artist} {title}")
        };
        if let Ok(results) = track_repo.search(&q, 1) {
            if let Some(track) = results.first() {
                if let Some(id) = track.id {
                    repo.add_tracks(playlist_id, &[id], None).ok();
                    matched += 1;
                }
            }
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": playlist_id,
            "name": name,
            "favorites_count": favorites.len(),
            "matched_tracks": matched,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Global Alarms CRUD
// ---------------------------------------------------------------------------

pub fn alarms_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_alarms).post(create_alarm_global))
        .route(
            "/{id}",
            axum::routing::put(update_alarm).delete(delete_alarm_global),
        )
        .route("/{id}/snooze", post(snooze_alarm))
}

async fn list_alarms(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, enabled, last_fired_at, created_at, fade_in_seconds FROM alarms ORDER BY time"
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "name": row.get::<_, Option<String>>(1).ok().flatten().unwrap_or_else(|| "Alarm".into()),
                    "time": row.get::<_, Option<String>>(2).ok().flatten(),
                    "days": row.get::<_, Option<String>>(3).ok().flatten(),
                    "one_shot": row.get::<_, i32>(4).unwrap_or(0) != 0,
                    "skip_holidays": row.get::<_, i32>(5).unwrap_or(0) != 0,
                    "zone_id": row.get::<_, Option<i64>>(6).ok().flatten(),
                    "source_type": row.get::<_, Option<String>>(7).ok().flatten(),
                    "source_id": row.get::<_, Option<String>>(8).ok().flatten(),
                    "source_name": row.get::<_, Option<String>>(9).ok().flatten(),
                    "volume": row.get::<_, Option<i32>>(10).ok().flatten(),
                    "fade_duration_s": row.get::<_, Option<i32>>(11).ok().flatten(),
                    "enabled": row.get::<_, i32>(12).unwrap_or(1) != 0,
                    "last_fired_at": row.get::<_, Option<String>>(13).ok().flatten(),
                    "created_at": row.get::<_, Option<String>>(14).ok().flatten(),
                    "fade_in_seconds": row.get::<_, Option<i32>>(15).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

#[derive(Deserialize)]
struct CreateAlarmGlobal {
    name: Option<String>,
    time: String,
    days: Option<String>,
    one_shot: Option<bool>,
    skip_holidays: Option<bool>,
    zone_id: Option<i64>,
    source_type: Option<String>,
    source_id: Option<String>,
    source_name: Option<String>,
    volume: Option<f64>,
    fade_duration_s: Option<i32>,
    fade_in_seconds: Option<i32>,
    enabled: Option<bool>,
}

async fn create_alarm_global(
    State(state): State<AppState>,
    Json(body): Json<CreateAlarmGlobal>,
) -> impl IntoResponse {
    let enabled_int: i32 = if body.enabled.unwrap_or(true) { 1 } else { 0 };
    let one_shot_int: i32 = if body.one_shot.unwrap_or(false) { 1 } else { 0 };
    let skip_holidays_int: i32 = if body.skip_holidays.unwrap_or(false) {
        1
    } else {
        0
    };

    match state.db.execute(
        "INSERT INTO alarms (name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, fade_in_seconds, enabled) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            &body.name.unwrap_or_else(|| "Alarm".into()) as &dyn rusqlite::types::ToSql,
            &body.time,
            &body.days.unwrap_or_else(|| "0,1,2,3,4".into()),
            &one_shot_int,
            &skip_holidays_int,
            &body.zone_id,
            &body.source_type,
            &body.source_id,
            &body.source_name,
            &body.volume.unwrap_or(0.3),
            &body.fade_duration_s.unwrap_or(60),
            &body.fade_in_seconds.unwrap_or(30),
            &enabled_int,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateAlarm {
    name: Option<String>,
    time: Option<String>,
    days: Option<String>,
    one_shot: Option<bool>,
    skip_holidays: Option<bool>,
    zone_id: Option<i64>,
    source_type: Option<String>,
    source_id: Option<String>,
    source_name: Option<String>,
    volume: Option<f64>,
    fade_duration_s: Option<i32>,
    fade_in_seconds: Option<i32>,
    enabled: Option<bool>,
}

async fn update_alarm(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateAlarm>,
) -> Result<impl IntoResponse, AppError> {
    // Build SET clause dynamically from provided fields
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql + Send>> = Vec::new();

    if let Some(ref name) = body.name {
        sets.push("name = ?".into());
        values.push(Box::new(name.clone()));
    }
    if let Some(ref time) = body.time {
        sets.push("time = ?".into());
        values.push(Box::new(time.clone()));
    }
    if let Some(ref days) = body.days {
        sets.push("days = ?".into());
        values.push(Box::new(days.clone()));
    }
    if let Some(one_shot) = body.one_shot {
        sets.push("one_shot = ?".into());
        values.push(Box::new(one_shot as i32));
    }
    if let Some(skip_holidays) = body.skip_holidays {
        sets.push("skip_holidays = ?".into());
        values.push(Box::new(skip_holidays as i32));
    }
    if let Some(zone_id) = body.zone_id {
        sets.push("zone_id = ?".into());
        values.push(Box::new(zone_id));
    }
    if let Some(ref source_type) = body.source_type {
        sets.push("source_type = ?".into());
        values.push(Box::new(source_type.clone()));
    }
    if let Some(ref source_id) = body.source_id {
        sets.push("source_id = ?".into());
        values.push(Box::new(source_id.clone()));
    }
    if let Some(ref source_name) = body.source_name {
        sets.push("source_name = ?".into());
        values.push(Box::new(source_name.clone()));
    }
    if let Some(volume) = body.volume {
        sets.push("volume = ?".into());
        values.push(Box::new(volume));
    }
    if let Some(fade_duration_s) = body.fade_duration_s {
        sets.push("fade_duration_s = ?".into());
        values.push(Box::new(fade_duration_s));
    }
    if let Some(fade_in_seconds) = body.fade_in_seconds {
        sets.push("fade_in_seconds = ?".into());
        values.push(Box::new(fade_in_seconds));
    }
    if let Some(enabled) = body.enabled {
        sets.push("enabled = ?".into());
        values.push(Box::new(enabled as i32));
    }

    if sets.is_empty() {
        return Ok((StatusCode::BAD_REQUEST, "no fields to update").into_response());
    }

    let sql = format!("UPDATE alarms SET {} WHERE id = ?", sets.join(", "));
    values.push(Box::new(id));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values
        .iter()
        .map(|v| v.as_ref() as &dyn rusqlite::types::ToSql)
        .collect();
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            drop(conn);
            Ok(StatusCode::NOT_FOUND.into_response())
        }
        Ok(_) => {
            drop(conn);
            Ok(Json(json!({ "id": id, "updated": true })).into_response())
        }
        Err(e) => {
            drop(conn);
            Ok((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
        }
    }
}

async fn delete_alarm_global(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.db.execute("DELETE FROM alarms WHERE id = ?", &[&id]) {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn snooze_alarm(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    match state.db.execute(
        "UPDATE alarms SET last_fired_at = NULL WHERE id = ?",
        &[&id],
    ) {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => Json(json!({ "id": id, "snoozed": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
