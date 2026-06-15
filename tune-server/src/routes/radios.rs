use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::radio_repo::{RadioRepo, RadioStation};

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
        .route("/add", get(add_from_web))
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
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn get_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
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
    let repo = RadioRepo::with_backend(state.backend.clone());
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
        Ok(id) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "created", "id": id}),
            );
            // Return the full station so the UI can display it immediately
            let created = repo.get(id).ok().flatten().unwrap_or(RadioStation {
                id: Some(id),
                ..station
            });
            (StatusCode::CREATED, Json(json!(created))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateRadioBody {
    name: Option<String>,
    #[serde(alias = "stream_url")]
    url: Option<String>,
    #[serde(alias = "homepage_url")]
    homepage: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    language: Option<String>,
    genre: Option<String>,
    codec: Option<String>,
    bitrate: Option<i32>,
    favorite: Option<bool>,
}

async fn update_radio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateRadioBody>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut station) = repo.get(id).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(name) = body.name {
        station.name = name;
    }
    if let Some(url) = body.url {
        station.url = url;
    }
    if let Some(homepage) = body.homepage {
        station.homepage = Some(homepage);
    }
    if let Some(logo_url) = body.logo_url {
        station.logo_url = Some(logo_url);
    }
    if let Some(country) = body.country {
        station.country = Some(country);
    }
    if let Some(language) = body.language {
        station.language = Some(language);
    }
    if let Some(genre) = body.genre {
        station.genre = Some(genre);
    }
    if let Some(codec) = body.codec {
        station.codec = Some(codec);
    }
    if let Some(bitrate) = body.bitrate {
        station.bitrate = Some(bitrate);
    }
    if let Some(fav) = body.favorite {
        station.is_favorite = fav;
        repo.set_favorite(id, fav).ok();
    }
    match repo.update(&station) {
        Ok(()) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "updated", "id": id}),
            );
            Json(json!(station)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "deleted", "id": id}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn play_radio(
    State(state): State<AppState>,
    Path((id, zone_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(radio) = repo.get(id).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "radio not found").into_response();
    };

    let play_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(radio.url.clone()),
        title: Some(radio.name.clone()),
        artist_name: Some("Live Radio".into()),
        album_title: Some("Live Radio".into()),
        cover_url: radio.logo_url.clone(),
        duration_ms: None,
    };

    let (output_sent, output_error) = match state.orchestrator.play(play_req).await {
        Ok(result) => (result.output_sent, result.error),
        Err(e) => (false, Some(e)),
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
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.search(&q.q).unwrap_or_default();
    Json(json!(items))
}

async fn list_favorites(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.favorites().unwrap_or_default();
    Json(json!(items))
}

#[derive(Deserialize)]
struct FavoriteToggle {
    favorite: Option<bool>,
}

#[derive(Deserialize)]
pub struct AddFromWebQuery {
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub country: Option<String>,
    pub logo_url: Option<String>,
}

async fn add_from_web(
    State(state): State<AppState>,
    Query(q): Query<AddFromWebQuery>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let station = RadioStation {
        id: None,
        name: q.name.clone(),
        url: q.url,
        homepage: None,
        logo_url: q.logo_url,
        country: q.country,
        language: None,
        genre: q.genre,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    let html = match repo.create(&station) {
        Ok(id) => {
            repo.set_favorite(id, true).ok();
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "created", "id": id}),
            );
            format!(
                r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Tune</title></head>
<body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center"><h1 style="color:#4ade80">✓ Radio ajoutée</h1><p><strong>{}</strong> a été ajoutée à vos favoris Tune.</p><p style="color:#888;margin-top:2em">Vous pouvez fermer cet onglet.</p></div>
</body></html>"#,
                q.name
            )
        }
        Err(e) => format!(
            r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Tune</title></head>
<body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center"><h1 style="color:#f87171">Erreur</h1><p>{e}</p></div>
</body></html>"#
        ),
    };
    axum::response::Html(html)
}

async fn toggle_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<FavoriteToggle>>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let current = repo.get(id).ok().flatten();
    let Some(current) = current else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let new_state = body
        .and_then(|b| b.favorite)
        .unwrap_or(!current.is_favorite);
    match repo.set_favorite(id, new_state) {
        Ok(_) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "favorite_toggled", "id": id, "favorite": new_state}),
            );
            Json(json!({ "id": id, "favorite": new_state })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Radio artwork / export / import
// ---------------------------------------------------------------------------

async fn set_radio_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut radio) = repo.get(id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio not found"})),
        )
            .into_response();
    };

    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "image" || name == "artwork" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                } else if ct.contains("webp") {
                    ext = "webp".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };

    let cache_dir = crate::routes::library::artwork_cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    let hash = tune_core::library::artwork::artwork_hash(&format!("radio-upload-{id}"));
    let path = cache_dir.join(format!("{hash}.{ext}"));
    if std::fs::write(&path, &data).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    radio.logo_url = Some(hash.clone());
    repo.update(&radio).ok();
    Json(json!(radio)).into_response()
}

async fn export_radios_m3u(State(state): State<AppState>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
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
    let repo = RadioRepo::with_backend(state.backend.clone());
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
    use tune_core::db::backend::ToSqlValue;
    let artist = body.artist.unwrap_or_default();
    let station = body.station_name.unwrap_or_default();
    match state.backend.execute(
        "INSERT OR IGNORE INTO radio_favorites (title, artist, station_name, cover_url, stream_url) VALUES (?, ?, ?, ?, ?)",
        &[&body.title as &dyn ToSqlValue, &artist as &dyn ToSqlValue, &station as &dyn ToSqlValue, &body.cover_url as &dyn ToSqlValue, &body.stream_url as &dyn ToSqlValue],
    ) {
        Ok(_) => {
            let id = state.backend.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio_favorite(
    State(state): State<AppState>,
    Path(fav_id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "DELETE FROM radio_favorites WHERE id = ?",
            &[&fav_id as &dyn ToSqlValue],
        )
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

    use tune_core::db::backend::ToSqlValue;
    match state.backend.execute(
        "INSERT OR IGNORE INTO radio_favorites (title, artist, station_name, cover_url) VALUES (?, ?, ?, ?)",
        &[&title as &dyn ToSqlValue, &artist as &dyn ToSqlValue, &station_name as &dyn ToSqlValue, &cover_url as &dyn ToSqlValue],
    ) {
        Ok(_) => {
            let id = state.backend.last_insert_rowid();
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
    let favorites: Vec<(String, String)> = state
        .backend
        .query_many(
            "SELECT title, artist FROM radio_favorites ORDER BY saved_at DESC",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            (
                r.get(0).and_then(|v| v.as_string()).unwrap_or_default(),
                r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            )
        })
        .collect();

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

    let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
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
    let rows = state.backend.query_many(
        "SELECT id, name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, enabled, last_fired_at, created_at, fade_in_seconds FROM alarms ORDER BY time",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "name": r.get(1).and_then(|v| v.as_string()).unwrap_or_else(|| "Alarm".into()),
                "time": r.get(2).and_then(|v| v.as_string()),
                "days": r.get(3).and_then(|v| v.as_string()),
                "one_shot": r.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                "skip_holidays": r.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                "zone_id": r.get(6).and_then(|v| v.as_i64()),
                "source_type": r.get(7).and_then(|v| v.as_string()),
                "source_id": r.get(8).and_then(|v| v.as_string()),
                "source_name": r.get(9).and_then(|v| v.as_string()),
                "volume": r.get(10).and_then(|v| v.as_i64()),
                "fade_duration_s": r.get(11).and_then(|v| v.as_i64()),
                "enabled": r.get(12).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "last_fired_at": r.get(13).and_then(|v| v.as_string()),
                "created_at": r.get(14).and_then(|v| v.as_string()),
                "fade_in_seconds": r.get(15).and_then(|v| v.as_i64()),
            })
        })
        .collect();
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

    use tune_core::db::backend::ToSqlValue;
    let name = body.name.unwrap_or_else(|| "Alarm".into());
    let days = body.days.unwrap_or_else(|| "0,1,2,3,4".into());
    let volume = body.volume.unwrap_or(0.3);
    let fade_duration_s = body.fade_duration_s.unwrap_or(60);
    let fade_in_seconds = body.fade_in_seconds.unwrap_or(30);
    match state.backend.execute(
        "INSERT INTO alarms (name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, fade_in_seconds, enabled) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[&name as &dyn ToSqlValue, &body.time as &dyn ToSqlValue, &days as &dyn ToSqlValue, &one_shot_int as &dyn ToSqlValue, &skip_holidays_int as &dyn ToSqlValue, &body.zone_id as &dyn ToSqlValue, &body.source_type as &dyn ToSqlValue, &body.source_id as &dyn ToSqlValue, &body.source_name as &dyn ToSqlValue, &volume as &dyn ToSqlValue, &fade_duration_s as &dyn ToSqlValue, &fade_in_seconds as &dyn ToSqlValue, &enabled_int as &dyn ToSqlValue],
    ) {
        Ok(_) => {
            let id = state.backend.last_insert_rowid();
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
    use tune_core::db::backend::{SqlValue, ToSqlValue};
    // Build SET clause dynamically from provided fields
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();

    if let Some(ref name) = body.name {
        sets.push("name = ?".into());
        values.push(name.to_sql_value());
    }
    if let Some(ref time) = body.time {
        sets.push("time = ?".into());
        values.push(time.to_sql_value());
    }
    if let Some(ref days) = body.days {
        sets.push("days = ?".into());
        values.push(days.to_sql_value());
    }
    if let Some(one_shot) = body.one_shot {
        sets.push("one_shot = ?".into());
        values.push((one_shot as i32).to_sql_value());
    }
    if let Some(skip_holidays) = body.skip_holidays {
        sets.push("skip_holidays = ?".into());
        values.push((skip_holidays as i32).to_sql_value());
    }
    if let Some(zone_id) = body.zone_id {
        sets.push("zone_id = ?".into());
        values.push(zone_id.to_sql_value());
    }
    if let Some(ref source_type) = body.source_type {
        sets.push("source_type = ?".into());
        values.push(source_type.to_sql_value());
    }
    if let Some(ref source_id) = body.source_id {
        sets.push("source_id = ?".into());
        values.push(source_id.to_sql_value());
    }
    if let Some(ref source_name) = body.source_name {
        sets.push("source_name = ?".into());
        values.push(source_name.to_sql_value());
    }
    if let Some(volume) = body.volume {
        sets.push("volume = ?".into());
        values.push(volume.to_sql_value());
    }
    if let Some(fade_duration_s) = body.fade_duration_s {
        sets.push("fade_duration_s = ?".into());
        values.push(fade_duration_s.to_sql_value());
    }
    if let Some(fade_in_seconds) = body.fade_in_seconds {
        sets.push("fade_in_seconds = ?".into());
        values.push(fade_in_seconds.to_sql_value());
    }
    if let Some(enabled) = body.enabled {
        sets.push("enabled = ?".into());
        values.push((enabled as i32).to_sql_value());
    }

    if sets.is_empty() {
        return Ok((StatusCode::BAD_REQUEST, "no fields to update").into_response());
    }

    let sql = format!("UPDATE alarms SET {} WHERE id = ?", sets.join(", "));
    values.push(id.to_sql_value());

    let params_ref: Vec<&dyn ToSqlValue> = values.iter().map(|v| v as &dyn ToSqlValue).collect();
    match state.backend.execute(&sql, &params_ref) {
        Ok(0) => Ok(StatusCode::NOT_FOUND.into_response()),
        Ok(_) => Ok(Json(json!({ "id": id, "updated": true })).into_response()),
        Err(e) => Ok((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
    }
}

async fn delete_alarm_global(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    match state
        .backend
        .execute("DELETE FROM alarms WHERE id = ?", &[&id as &dyn ToSqlValue])
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn snooze_alarm(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    match state.backend.execute(
        "UPDATE alarms SET last_fired_at = NULL WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    ) {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => Json(json!({ "id": id, "snoozed": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
