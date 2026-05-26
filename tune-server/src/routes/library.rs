use axum::body::Body;
use lofty::file::TaggedFileExt;
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
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::rating_repo::RatingRepo;

use crate::state::AppState;

const MB_USER_AGENT: &str = "Tune/2.0 (https://mozaiklabs.fr)";

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
#[allow(dead_code)]
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
        .route("/artists/{id}/bio", get(artist_bio))
        .route("/artists/{id}/similar", get(artist_similar))
        .route("/artists/{id}/metadata", get(artist_metadata))
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
        .route("/tracks/{id}/quick-fav", post(quick_fav_track))
        .route("/albums/{id}/quick-fav", post(quick_fav_album))
        .route("/genre-tree", get(genre_tree).put(update_genre_tree))
        .route("/albums/top-rated", get(top_rated_albums))
        .route("/albums/{id}/rate", post(rate_album))
        .route("/albums/{id}/rating", get(get_album_rating))
        .route("/tracks/{id}/credits", get(track_credits))
        .route("/artists/{id}/credits", get(artist_credits))
        .route("/tracks/{id}/credits/enrich", post(enrich_track_credits))
        .route("/albums/{id}/credits/enrich", post(enrich_album_credits))
        .route("/enrich-credits", post(enrich_all_credits))
        .route("/tracks/{id}/all-tags", get(track_all_tags))
        .route("/browse", get(browse_roots))
        .route("/search", get(search))
        .route("/stats", get(library_stats))
        .route("/artwork/{hash}", get(serve_artwork))
        .route("/artwork/proxy", get(proxy_artwork))
        .route("/albums/{id}/artwork", get(album_artwork))
        .route("/albums/{id}/artwork/enrich", post(enrich_album_artwork))
}

async fn list_artists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = ArtistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let _total = repo.count().unwrap_or(0);
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

#[derive(Deserialize)]
struct LangQuery {
    lang: Option<String>,
}

async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else { return StatusCode::NOT_FOUND.into_response(); };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "bio": null, "error": "no MusicBrainz ID"})).into_response();
    };
    let lang = q.lang.as_deref().unwrap_or("fr");
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    match client.get(format!("https://mozaiklabs.fr/api/{mbid}/bio?lang={lang}")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "bio": null})).into_response(),
    }
}

async fn artist_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else { return StatusCode::NOT_FOUND.into_response(); };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "artists": []})).into_response();
    };
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    match client.get(format!("https://mozaiklabs.fr/api/{mbid}/similar")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

async fn artist_metadata(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else { return StatusCode::NOT_FOUND.into_response(); };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!(artist)).into_response();
    };
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    match client.get(format!("https://mozaiklabs.fr/api/{mbid}")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!(artist)).into_response(),
    }
}

#[derive(Deserialize)]
struct QuickFavQuery {
    profile_id: Option<i64>,
}

async fn quick_fav_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or(1);
    let repo = ProfileRepo::new(state.db);
    let is_fav = repo.is_favorite(profile_id, "track", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "track", id).ok();
    } else {
        repo.add_favorite(profile_id, "track", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "track_id": id}))
}

async fn quick_fav_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or(1);
    let repo = ProfileRepo::new(state.db);
    let is_fav = repo.is_favorite(profile_id, "album", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "album", id).ok();
    } else {
        repo.add_favorite(profile_id, "album", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "album_id": id}))
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
    let _total = repo.count().unwrap_or(0);
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
        .and_then(tune_core::audio::formats::AudioFormat::from_extension)
        .map(|f| f.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_str(&mime).unwrap());
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

    let path_owned = file_path.clone();
    let body = Body::from_stream(async_stream::stream! {
        if let Ok(mut file) = tokio::fs::File::open(&path_owned).await {
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

async fn update_genre_tree(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    settings.set("genre_tree", &body.to_string()).ok();
    Json(json!({"updated": true}))
}

async fn serve_artwork(Path(hash): Path<String>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    for ext in &["jpg", "png"] {
        let path = cache_dir.join(format!("{hash}.{ext}"));
        if path.exists()
            && let Ok(data) = tokio::fs::read(&path).await {
                let mime = if *ext == "png" { "image/png" } else { "image/jpeg" };
                let mut headers = axum::http::HeaderMap::new();
                headers.insert("Content-Type", axum::http::HeaderValue::from_static(mime));
                headers.insert("Cache-Control", axum::http::HeaderValue::from_static("public, max-age=86400"));
                return (StatusCode::OK, headers, data).into_response();
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
    if let Some(track) = tracks.first()
        && let Some(ref file_path) = track.file_path {
            let cache_dir = artwork_cache_dir();
            if let Some(hash) = tune_core::artwork::get_or_extract(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response();
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

#[derive(Deserialize)]
struct ProxyQuery {
    url: String,
}

async fn proxy_artwork(Query(q): Query<ProxyQuery>) -> impl IntoResponse {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();

    match client.get(&q.url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            match resp.bytes().await {
                Ok(data) => {
                    let mut headers = HeaderMap::new();
                    headers.insert("Content-Type", HeaderValue::from_str(&content_type).unwrap_or(HeaderValue::from_static("image/jpeg")));
                    headers.insert("Cache-Control", HeaderValue::from_static("public, max-age=86400"));
                    (StatusCode::OK, headers, data.to_vec()).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

// --- Credit enrichment handlers ---

async fn track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT id, track_id, artist_id, artist_name, role, instrument, position FROM track_credits WHERE track_id = ? ORDER BY position")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![id], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "track_id": row.get::<_, Option<i64>>(1).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(3).ok().flatten(),
                    "role": row.get::<_, Option<String>>(4).ok().flatten(),
                    "instrument": row.get::<_, Option<String>>(5).ok().flatten(),
                    "position": row.get::<_, Option<i32>>(6).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

async fn artist_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT tc.id, tc.track_id, tc.artist_id, tc.artist_name, tc.role, tc.instrument, tc.position \
             FROM track_credits tc \
             WHERE tc.artist_id = ? OR tc.artist_name = (SELECT name FROM artists WHERE id = ?) \
             ORDER BY tc.track_id, tc.position"
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![id, id], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "track_id": row.get::<_, Option<i64>>(1).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(3).ok().flatten(),
                    "role": row.get::<_, Option<String>>(4).ok().flatten(),
                    "instrument": row.get::<_, Option<String>>(5).ok().flatten(),
                    "position": row.get::<_, Option<i32>>(6).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

async fn enrich_track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return Json(json!({"enriched": false, "reason": "track not found"})).into_response(),
    };

    let Some(ref mbid) = track.musicbrainz_recording_id else {
        return Json(json!({"enriched": false, "reason": "no MusicBrainz recording ID"})).into_response();
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(MB_USER_AGENT)
        .build()
        .unwrap();

    let url = format!(
        "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
    );

    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(data) => data,
            Err(_) => return Json(json!({"enriched": false, "reason": "invalid MusicBrainz response"})).into_response(),
        },
        Ok(r) => return Json(json!({"enriched": false, "reason": format!("MusicBrainz HTTP {}", r.status())})).into_response(),
        Err(e) => return Json(json!({"enriched": false, "reason": format!("MusicBrainz request failed: {e}")})).into_response(),
    };

    // Clear existing credits for this track
    state.db.execute(
        "DELETE FROM track_credits WHERE track_id = ?",
        &[&id as &dyn rusqlite::types::ToSql],
    ).ok();

    let mut count = 0i32;

    // Parse artist-credits
    if let Some(credits) = resp.get("artist-credit").and_then(|v| v.as_array()) {
        for (pos, credit) in credits.iter().enumerate() {
            let artist_name = credit.get("name")
                .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown");
            state.db.execute(
                "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                &[&id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
            ).ok();
            count += 1;
        }
    }

    // Parse relations for performer/instrument roles
    if let Some(relations) = resp.get("relations").and_then(|v| v.as_array()) {
        for rel in relations {
            let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let artist_name = rel.get("artist").and_then(|a| a.get("name")).and_then(|v| v.as_str());
            if let Some(name) = artist_name {
                let instrument = rel.get("attributes")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                state.db.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, ?)",
                    &[
                        &id as &dyn rusqlite::types::ToSql,
                        &name,
                        &rel_type,
                        &instrument,
                        &count,
                    ],
                ).ok();
                count += 1;
            }
        }
    }

    Json(json!({"enriched": true, "credits_count": count})).into_response()
}

async fn enrich_album_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::new(state.db.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();

    let mut enriched = 0i32;
    let mut skipped = 0i32;
    let mut failed = 0i32;
    let total = tracks.len() as i32;

    for track in &tracks {
        let track_id = match track.id {
            Some(id) => id,
            None => { skipped += 1; continue; }
        };

        let Some(ref mbid) = track.musicbrainz_recording_id else {
            skipped += 1;
            continue;
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(MB_USER_AGENT)
            .build()
            .unwrap();

        let url = format!(
            "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
        );

        let resp = match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(data) => data,
                Err(_) => { failed += 1; continue; }
            },
            _ => { failed += 1; continue; }
        };

        state.db.execute(
            "DELETE FROM track_credits WHERE track_id = ?",
            &[&track_id as &dyn rusqlite::types::ToSql],
        ).ok();

        if let Some(credits) = resp.get("artist-credit").and_then(|v| v.as_array()) {
            for (pos, credit) in credits.iter().enumerate() {
                let artist_name = credit.get("name")
                    .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                state.db.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                    &[&track_id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
                ).ok();
            }
        }

        if let Some(relations) = resp.get("relations").and_then(|v| v.as_array()) {
            for rel in relations {
                let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let artist_name = rel.get("artist").and_then(|a| a.get("name")).and_then(|v| v.as_str());
                if let Some(name) = artist_name {
                    let instrument = rel.get("attributes")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    state.db.execute(
                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                        &[&track_id as &dyn rusqlite::types::ToSql, &name, &rel_type, &instrument],
                    ).ok();
                }
            }
        }

        enriched += 1;

        // MusicBrainz rate limit: 1 request/sec
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    Json(json!({
        "album_id": id,
        "total": total,
        "enriched": enriched,
        "skipped": skipped,
        "failed": failed,
    }))
}

async fn enrich_all_credits(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let task_id_clone = task_id.clone();
    let db = state.db.clone();

    tokio::spawn(async move {
        let track_ids: Vec<(i64, String)> = {
            let conn = db.connection().lock().unwrap();
            conn
                .prepare("SELECT id, musicbrainz_recording_id FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
        };

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(MB_USER_AGENT)
            .build()
            .unwrap();

        let mut enriched = 0i32;
        for (track_id, mbid) in &track_ids {
            let url = format!(
                "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
            );

            if let Ok(r) = client.get(&url).send().await {
                if r.status().is_success() {
                    if let Ok(data) = r.json::<Value>().await {
                        db.execute(
                            "DELETE FROM track_credits WHERE track_id = ?",
                            &[track_id as &dyn rusqlite::types::ToSql],
                        ).ok();

                        if let Some(credits) = data.get("artist-credit").and_then(|v| v.as_array()) {
                            for (pos, credit) in credits.iter().enumerate() {
                                let artist_name = credit.get("name")
                                    .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                db.execute(
                                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                                    &[track_id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
                                ).ok();
                            }
                        }

                        if let Some(relations) = data.get("relations").and_then(|v| v.as_array()) {
                            for rel in relations {
                                let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                let artist_name = rel.get("artist").and_then(|a| a.get("name")).and_then(|v| v.as_str());
                                if let Some(name) = artist_name {
                                    let instrument = rel.get("attributes")
                                        .and_then(|v| v.as_array())
                                        .and_then(|arr| arr.first())
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string());
                                    db.execute(
                                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                                        &[track_id as &dyn rusqlite::types::ToSql, &name, &rel_type, &instrument],
                                    ).ok();
                                }
                            }
                        }

                        enriched += 1;
                    }
                }
            }

            // MusicBrainz rate limit: 1 request/sec
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }

        tracing::info!(task_id = %task_id_clone, enriched, total = track_ids.len(), "enrich_all_credits_done");
    });

    (StatusCode::ACCEPTED, Json(json!({"status": "accepted", "task_id": task_id})))
}

async fn track_all_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db);
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let mut result = serde_json::to_value(&track).unwrap_or_default();

    // Try reading raw file tags with lofty
    if let Some(ref path) = track.file_path {
        if let Ok(tagged) = lofty::read_from_path(path) {
            let tags: Vec<Value> = tagged.tags().iter().map(|tag| {
                json!({
                    "tag_type": format!("{:?}", tag.tag_type()),
                    "items": tag.items().map(|item| format!("{:?}", item)).collect::<Vec<_>>(),
                })
            }).collect();
            result["file_tags"] = json!(tags);
        }
    }

    Json(result).into_response()
}

async fn enrich_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return (StatusCode::NOT_FOUND, Json(json!({"error": "album not found"}))).into_response(),
    };

    // Skip if album already has a cover
    if album.cover_path.is_some() {
        return Json(json!({"enriched": false, "reason": "album already has cover art"})).into_response();
    }

    let Some(ref mbid) = album.musicbrainz_release_id else {
        return Json(json!({"enriched": false, "reason": "no MusicBrainz release ID"})).into_response();
    };

    match tune_core::artwork::fetch_cover_art(mbid).await {
        Some(data) => {
            let cache_dir = artwork_cache_dir();
            let hash = tune_core::artwork::artwork_hash(mbid);
            if tune_core::artwork::save_to_cache(&data, &cache_dir, &hash, "jpg").is_some() {
                repo.update_cover_path(id, &hash).ok();
                Json(json!({"enriched": true, "hash": hash, "size": data.len()})).into_response()
            } else {
                Json(json!({"enriched": false, "reason": "failed to save to cache"})).into_response()
            }
        }
        None => {
            Json(json!({"enriched": false, "reason": "no cover art found on Cover Art Archive"})).into_response()
        }
    }
}

pub(crate) fn artwork_cache_dir() -> std::path::PathBuf {
    let dir = std::env::var("TUNE_ARTWORK_DIR")
        .unwrap_or_else(|_| "artwork_cache".into());
    std::path::PathBuf::from(dir)
}
