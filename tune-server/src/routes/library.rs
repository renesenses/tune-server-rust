use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::rating_repo::RatingRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct Pagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct AlbumFilters {
    limit: Option<i64>,
    offset: Option<i64>,
    quality: Option<String>,
    format: Option<String>,
    sort: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/artists", get(list_artists))
        .route("/artists/{id}", get(get_artist))
        .route("/artists/{id}/albums", get(artist_albums))
        .route("/artists/{id}/tracks", get(artist_tracks))
        .route("/albums", get(list_albums))
        .route("/albums/count", get(album_count))
        .route("/albums/filters", get(album_filters))
        .route("/albums/recent", get(recent_albums))
        .route("/albums/{id}", get(get_album))
        .route("/albums/{id}/tracks", get(album_tracks))
        .route("/tracks", get(list_tracks))
        .route("/tracks/count", get(track_count))
        .route("/tracks/{id}", get(get_track))
        .route("/tracks/{id}/audio", get(stream_track_audio))
        .route("/tracks/{id}/rescan", post(rescan_track))
        .route("/genre-tree", get(genre_tree))
        .route("/albums/top-rated", get(top_rated_albums))
        .route("/albums/{id}/rate", post(rate_album))
        .route("/albums/{id}/rating", get(get_album_rating))
        .route("/browse", get(browse_roots))
        .route("/search", get(search))
        .route("/stats", get(library_stats))
        .route("/artwork/{hash}", get(serve_artwork))
        .route("/albums/{id}/artwork", get(album_artwork))
}

async fn list_artists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = ArtistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    Json(json!(items))
}

async fn get_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(artist)) => Json(json!(artist)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn artist_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = AlbumRepo::new(state.db);
    let items = repo.list_by_artist(id).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

async fn artist_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::new(state.db);
    let items = repo.list_by_artist(id).unwrap_or_default();
    Json(json!(items))
}

async fn list_albums(
    State(state): State<AppState>,
    Query(p): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = AlbumRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

async fn album_count(State(state): State<AppState>) -> Json<Value> {
    let count = AlbumRepo::new(state.db).count().unwrap_or(0);
    Json(json!({ "count": count }))
}

async fn album_filters(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let formats: Vec<String> = conn
        .prepare("SELECT DISTINCT format FROM albums WHERE format IS NOT NULL ORDER BY format")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    let sample_rates: Vec<i32> = conn
        .prepare("SELECT DISTINCT sample_rate FROM albums WHERE sample_rate IS NOT NULL ORDER BY sample_rate")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!({ "formats": formats, "sample_rates": sample_rates }))
}

async fn recent_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let repo = AlbumRepo::new(state.db);
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

async fn get_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(album)) => Json(album.to_json()).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn album_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::new(state.db);
    let items = repo.list_by_album(id).unwrap_or_default();
    Json(json!(items))
}

async fn list_tracks(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = TrackRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    Json(json!(items))
}

async fn track_count(State(state): State<AppState>) -> Json<Value> {
    let count = TrackRepo::new(state.db).count().unwrap_or(0);
    Json(json!({ "count": count }))
}

async fn get_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(track)) => Json(json!(track)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn stream_track_audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    _req_headers: HeaderMap,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let path = std::path::Path::new(file_path);
    let file_size = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let mime = track
        .format
        .as_deref()
        .and_then(|f| tune_core::audio::formats::AudioFormat::from_extension(f))
        .map(|f| f.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_str(&mime).unwrap());
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

    let path_owned = file_path.clone();
    let body = Body::from_stream(async_stream::stream! {
        match tokio::fs::File::open(&path_owned).await {
            Ok(mut file) => {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 65536];
                loop {
                    match file.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])),
                        Err(_e) => { break; }
                    }
                }
            }
            Err(_) => {}
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

async fn rescan_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return (StatusCode::BAD_REQUEST, "no file path").into_response();
    };

    let meta = tune_core::metadata::read_metadata(std::path::Path::new(file_path));
    match meta {
        Some(m) => Json(json!({
            "title": m.title,
            "artist": m.artist,
            "album": m.album,
            "sample_rate": m.sample_rate,
            "bit_depth": m.bit_depth,
            "duration_ms": m.duration_ms,
        }))
        .into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read metadata").into_response(),
    }
}

async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(20);
    let artists = ArtistRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    let tracks = TrackRepo::new(state.db)
        .search(&q.q, limit)
        .unwrap_or_default();

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
    }))
}

async fn library_stats(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let listens = HistoryRepo::new(state.db.clone()).count().unwrap_or(0);

    let conn = state.db.connection().lock().unwrap();
    let total_duration_ms: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(duration_ms), 0) FROM tracks",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let total_size_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(file_size), 0) FROM tracks WHERE file_size IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "total_duration_ms": total_duration_ms,
        "total_size_bytes": total_size_bytes,
    }))
}

#[derive(Deserialize)]
struct RateRequest {
    rating: i32,
    note: Option<String>,
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
struct RatingQuery {
    profile_id: Option<i64>,
}

async fn rate_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RateRequest>,
) -> impl IntoResponse {
    let repo = RatingRepo::new(state.db);
    let profile_id = body.profile_id.unwrap_or(1);
    match repo.rate_album(id, profile_id, body.rating, body.note.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn get_album_rating(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RatingQuery>,
) -> impl IntoResponse {
    let repo = RatingRepo::new(state.db);
    let profile_id = q.profile_id.unwrap_or(1);
    match repo.get_rating(id, profile_id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => Json(json!({ "rating": null, "album_id": id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn top_rated_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = RatingRepo::new(state.db.clone());
    let album_repo = AlbumRepo::new(state.db);
    let top = repo.top_rated(limit).unwrap_or_default();

    let items: Vec<Value> = top
        .iter()
        .filter_map(|(album_id, avg_rating, count)| {
            let album = album_repo.get(*album_id).ok()??;
            Some(json!({
                "album": album,
                "avg_rating": avg_rating,
                "rating_count": count,
            }))
        })
        .collect();

    Json(json!(items))
}

async fn genre_tree(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let genres: Vec<String> = conn
        .prepare("SELECT DISTINCT genre FROM tracks WHERE genre IS NOT NULL AND genre != '' ORDER BY genre COLLATE NOCASE")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    let mut tree: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for genre in &genres {
        let parts: Vec<&str> = genre.splitn(2, '/').collect();
        let parent = parts[0].trim().to_string();
        if parts.len() > 1 {
            tree.entry(parent).or_default().push(parts[1].trim().to_string());
        } else {
            tree.entry(parent).or_default();
        }
    }

    Json(json!({
        "tree": tree,
        "genres": genres,
        "total": genres.len(),
    }))
}

async fn serve_artwork(Path(hash): Path<String>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    for ext in &["jpg", "png"] {
        let path = cache_dir.join(format!("{hash}.{ext}"));
        if path.exists() {
            if let Ok(data) = tokio::fs::read(&path).await {
                let mime = if *ext == "png" { "image/png" } else { "image/jpeg" };
                let mut headers = axum::http::HeaderMap::new();
                headers.insert("Content-Type", axum::http::HeaderValue::from_static(mime));
                headers.insert("Cache-Control", axum::http::HeaderValue::from_static("public, max-age=86400"));
                return (StatusCode::OK, headers, data).into_response();
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref cover_path) = album.cover_path {
        if cover_path.starts_with("http") {
            return axum::response::Redirect::temporary(cover_path).into_response();
        }
        let hash = tune_core::artwork::artwork_hash(cover_path);
        return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response();
    }

    let track_repo = TrackRepo::new(state.db);
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if let Some(track) = tracks.first() {
        if let Some(ref file_path) = track.file_path {
            let cache_dir = artwork_cache_dir();
            if let Some(hash) = tune_core::artwork::get_or_extract(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response();
            }
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn browse_roots(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let roots: Vec<Value> = dirs
        .iter()
        .map(|d| json!({ "path": d, "name": d, "track_count": 0 }))
        .collect();
    Json(json!({ "roots": roots }))
}

pub(crate) fn artwork_cache_dir() -> std::path::PathBuf {
    let dir = std::env::var("TUNE_ARTWORK_DIR")
        .unwrap_or_else(|_| "artwork_cache".into());
    std::path::PathBuf::from(dir)
}
