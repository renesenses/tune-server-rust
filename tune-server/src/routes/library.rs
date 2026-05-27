use axum::body::Body;
use lofty::file::TaggedFileExt;
use axum::extract::{Multipart, Path, Query, State};
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
use tune_core::db::zone_repo::ZoneRepo;

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
struct AlbumFilters {
    limit: Option<i64>,
    offset: Option<i64>,
    quality: Option<String>,
    format: Option<String>,
    sort: Option<String>,
    order: Option<String>,
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
        .route("/browse/dir", get(browse_directory))
        .route("/folders", get(browse_folders))
        .route("/genres", get(list_genres))
        .route("/genres/{name}/albums", get(genre_albums))
        .route("/recommendations", get(recommendations))
        .route("/stats/completeness", get(completeness_stats))
        .route("/search", get(search))
        .route("/stats", get(library_stats))
        .route("/artwork/{hash}", get(serve_artwork))
        .route("/artwork/proxy", get(proxy_artwork))
        .route("/albums/{id}/artwork", get(album_artwork))
        .route("/albums/{id}/artwork/enrich", post(enrich_album_artwork))
        .route("/artwork/enrich", post(batch_enrich_artwork))
        .route("/artwork/enrich/status", get(batch_enrich_artwork_status))
        .route("/duplicates", get(list_duplicates))
        .route("/duplicates/resolve", post(resolve_duplicate))
        .route("/activity", get(library_activity))
        .route("/albums/{id}/bio", get(album_bio))
        .route("/albums/{id}/similar", get(album_similar))
        .route("/albums/{id}/artwork/rescan", post(rescan_album_artwork))
        .route("/albums/merge-duplicates", post(merge_duplicate_albums_route))
        .route("/artists/{id}/timeline", get(artist_timeline))
        .route("/artists/{id}/image/upload", post(artist_image_upload))
        .route("/artists/{id}/image/report", post(artist_image_report))
        .route("/tracks/{id}/lyrics", get(track_lyrics))
        .route("/ratings/export", get(export_ratings))
        .route("/ratings/import", post(import_ratings))
        .route("/enrich-all", post(enrich_all_library))
        .route("/enrich-all/status", get(enrich_all_status))
        .route("/artwork/rescan", post(rescan_all_artwork))
        .route("/duplicates/smart", get(smart_duplicates))
        .route("/collections", get(list_collections).post(create_collection))
        .route("/collections/{id}", get(get_collection).delete(delete_collection))
        .route("/collections/{id}/albums", get(collection_albums))
        .route("/collections/{id}/albums/{album_id}", post(add_album_to_collection).delete(remove_album_from_collection))
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
    let sort = p.sort.as_deref().unwrap_or("added_at");
    let order = p.order.as_deref().unwrap_or("asc");
    let items = repo.list_sorted(limit, offset, sort, order).unwrap_or_default();
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
    let zones = ZoneRepo::new(state.db.clone()).count().unwrap_or(0);

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
        "zones": zones,
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
    // Collect all individual genres from both the `genres` JSON array
    // and the legacy `genre` text column (splitting multi-genre strings).
    let raw_genres: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '') GROUP BY genre, genres")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).unwrap_or(None),
                    row.get::<_, Option<String>>(1).unwrap_or(None),
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();
    drop(conn);

    let mut genre_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (genre_col, genres_col) in &raw_genres {
        // Prefer the structured genres JSON array if present
        if let Some(json_str) = genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                for g in arr {
                    let trimmed = g.trim().to_string();
                    if !trimmed.is_empty() {
                        genre_set.insert(trimmed);
                    }
                }
                continue;
            }
        }
        // Fall back to splitting the legacy genre column
        if let Some(raw) = genre_col {
            for g in tune_core::metadata::split_genre_tag(raw) {
                if !g.is_empty() {
                    genre_set.insert(g);
                }
            }
        }
    }

    let genres: Vec<String> = genre_set.into_iter().collect();
    let mut tree: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for genre in &genres {
        tree.entry(genre.clone()).or_default();
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
                headers.insert("Cache-Control", axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"));
                headers.insert("ETag", axum::http::HeaderValue::from_str(&format!("\"{hash}\"")).unwrap_or(axum::http::HeaderValue::from_static("\"artwork\"")));
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
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let conn = state.db.connection().lock().unwrap();
    let roots: Vec<Value> = dirs
        .iter()
        .map(|d| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tracks WHERE file_path LIKE ?",
                    rusqlite::params![format!("{d}%")],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let name = std::path::Path::new(d)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(d);
            json!({ "path": d, "name": name, "track_count": count })
        })
        .collect();
    drop(conn);
    Json(json!({ "roots": roots }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    path: String,
}

async fn browse_directory(
    State(state): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> impl IntoResponse {
    let resolved = std::path::Path::new(&q.path);
    if !resolved.is_absolute() || !resolved.exists() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid path"}))).into_response();
    }

    // Verify path is under a configured music dir
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let music_root = dirs.iter().find(|d| q.path.starts_with(d.as_str()));
    if music_root.is_none() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "path not under a configured music directory"}))).into_response();
    }
    let music_root = music_root.unwrap().clone();

    // List subdirectories
    let mut subdirs: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&q.path) {
        let conn = state.db.connection().lock().unwrap();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_path = path.to_string_lossy().to_string();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                if name.starts_with('.') { continue; }
                let track_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE ?",
                        rusqlite::params![format!("{dir_path}%")],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                subdirs.push(json!({ "name": name, "path": dir_path, "track_count": track_count }));
            }
        }
        drop(conn);
    }
    subdirs.sort_by(|a, b| {
        a.get("name").and_then(|v| v.as_str()).unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });

    // List tracks in this directory (not recursive — only direct children)
    let conn = state.db.connection().lock().unwrap();
    // Use a LIKE pattern that matches the directory prefix and filter in app
    // for direct children only.
    let dir_prefix = format!("{}%", q.path);
    let tracks: Vec<Value> = conn
        .prepare("SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, t.disc_number, t.track_number, t.duration_ms, t.file_path, t.format, t.sample_rate, t.bit_depth, t.genre, t.year, al.cover_path FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id WHERE t.file_path LIKE ? ORDER BY t.disc_number, t.track_number, t.title")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![dir_prefix], |row| {
                let file_path: Option<String> = row.get(9).ok();
                let is_direct = file_path.as_ref().map(|fp| {
                    let parent = std::path::Path::new(fp).parent().and_then(|p| p.to_str()).unwrap_or("");
                    parent == q.path
                }).unwrap_or(false);
                Ok((is_direct, json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "album_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(4).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(5).ok().flatten(),
                    "disc_number": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "track_number": row.get::<_, Option<i32>>(7).ok().flatten(),
                    "duration_ms": row.get::<_, Option<i64>>(8).ok().flatten(),
                    "file_path": file_path,
                    "format": row.get::<_, Option<String>>(10).ok().flatten(),
                    "sample_rate": row.get::<_, Option<i32>>(11).ok().flatten(),
                    "bit_depth": row.get::<_, Option<i32>>(12).ok().flatten(),
                    "genre": row.get::<_, Option<String>>(13).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(14).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(15).ok().flatten(),
                })))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).filter(|(direct, _)| *direct).map(|(_, v)| v).collect())
        })
        .unwrap_or_default();
    drop(conn);

    // Parent path
    let parent = if q.path != music_root {
        resolved.parent().map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    Json(json!({
        "path": q.path,
        "parent": parent,
        "music_root": music_root,
        "directories": subdirs,
        "tracks": tracks,
    })).into_response()
}

#[derive(Deserialize)]
struct FolderQuery {
    path: Option<String>,
}

async fn browse_folders(
    State(state): State<AppState>,
    Query(q): Query<FolderQuery>,
) -> axum::response::Response {
    // /library/folders?path=... is an alias for browse_directory
    // Without a path param, return browse roots
    match q.path {
        Some(ref p) if !p.is_empty() => {
            browse_directory(State(state), Query(BrowseQuery { path: p.clone() })).await.into_response()
        }
        _ => {
            let roots_json = browse_roots(State(state)).await;
            roots_json.into_response()
        }
    }
}

#[derive(Deserialize)]
struct GenreQuery {
    query: Option<String>,
}

async fn list_genres(
    State(state): State<AppState>,
    Query(params): Query<GenreQuery>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    // Collect genre + genres columns from all albums
    let raw: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT genre, genres FROM albums WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).unwrap_or(None),
                    row.get::<_, Option<String>>(1).unwrap_or(None),
                ))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    // Split multi-genre values and count individual genres
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for (genre_col, genres_col) in &raw {
        let mut genres_for_album: Vec<String> = Vec::new();
        // Prefer the structured genres JSON array if present
        if let Some(json_str) = genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                genres_for_album = arr.into_iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
            }
        }
        // Fall back to splitting the legacy genre column
        if genres_for_album.is_empty() {
            if let Some(raw_genre) = genre_col {
                genres_for_album = tune_core::metadata::split_genre_tag(raw_genre);
            }
        }
        for g in genres_for_album {
            *counts.entry(g).or_insert(0) += 1;
        }
    }

    // Filter by query parameter (case-insensitive LIKE match)
    let filter = params.query.map(|q| q.to_lowercase());

    let items: Vec<Value> = counts
        .iter()
        .filter(|(name, _)| {
            match &filter {
                Some(q) => name.to_lowercase().contains(q),
                None => true,
            }
        })
        .map(|(name, count)| json!({ "name": name, "count": count }))
        .collect();

    Json(json!(items))
}

async fn genre_albums(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    let decoded = urlencoding::decode(&name).unwrap_or_else(|_| name.clone().into());
    let repo = AlbumRepo::new(state.db);
    let items = repo.list_by_genre(&decoded).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

async fn recommendations(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    // Return recently added albums the user hasn't listened to
    let limit = p.limit.unwrap_or(20);
    let repo = AlbumRepo::new(state.db);
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!({ "albums": items }))
}

async fn completeness_stats(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let total_tracks: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0)).unwrap_or(0);
    let with_genre: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE genre IS NOT NULL AND genre != ''", [], |row| row.get(0)).unwrap_or(0);
    let with_year: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE year IS NOT NULL", [], |row| row.get(0)).unwrap_or(0);
    let with_artist: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE artist_id IS NOT NULL", [], |row| row.get(0)).unwrap_or(0);
    let with_album: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE album_id IS NOT NULL", [], |row| row.get(0)).unwrap_or(0);
    let with_cover: i64 = conn.query_row("SELECT COUNT(DISTINCT a.id) FROM albums a WHERE a.cover_path IS NOT NULL AND a.cover_path != ''", [], |row| row.get(0)).unwrap_or(0);
    let total_albums: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0)).unwrap_or(0);
    let with_mbid: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''", [], |row| row.get(0)).unwrap_or(0);
    drop(conn);

    let total_artists: i64 = {
        let conn = state.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM artists", [], |row| row.get(0)).unwrap_or(0)
    };

    Json(json!({
        "total_tracks": total_tracks,
        "total_albums": total_albums,
        "total_artists": total_artists,
        "with_genre": with_genre,
        "with_year": with_year,
        "with_artist": with_artist,
        "with_album": with_album,
        "with_cover": with_cover,
        "with_musicbrainz_id": with_mbid,
        "albums_without_cover": total_albums - with_cover,
        "albums_without_genre": total_albums - (with_genre * total_albums / total_tracks.max(1)),
        "albums_without_year": total_albums - (with_year * total_albums / total_tracks.max(1)),
        "tracks_without_artist": total_tracks - with_artist,
        "artists_without_image": total_artists,
        "doubtful_count": 0,
        "genre_pct": if total_tracks > 0 { (with_genre as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "year_pct": if total_tracks > 0 { (with_year as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "artist_pct": if total_tracks > 0 { (with_artist as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "album_pct": if total_tracks > 0 { (with_album as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "cover_pct": if total_albums > 0 { (with_cover as f64 / total_albums as f64 * 100.0).round() } else { 0.0 },
        "mbid_pct": if total_tracks > 0 { (with_mbid as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
    }))
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

async fn batch_enrich_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.db.clone();

    // Check how many albums are missing covers
    let album_repo = AlbumRepo::new(state.db.clone());
    let missing = album_repo.list_without_cover().unwrap_or_default();

    if missing.is_empty() {
        return Json(json!({
            "status": "skipped",
            "message": "all albums already have cover art",
            "missing": 0,
        })).into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    settings.set("artwork_enrich_status", "running").ok();
    settings.set("artwork_enrich_result", &json!({"total": missing.len(), "enriched": 0, "status": "running"}).to_string()).ok();

    tokio::spawn(async move {
        tune_core::artwork::batch_enrich_artwork(db, cache_dir).await;
    });

    (StatusCode::ACCEPTED, Json(json!({
        "status": "accepted",
        "message": "batch artwork enrichment started",
        "albums_to_process": missing.len(),
    }))).into_response()
}

async fn batch_enrich_artwork_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let result = settings
        .get("artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let album_repo = AlbumRepo::new(state.db);
    let still_missing = album_repo.list_without_cover().unwrap_or_default().len();

    Json(json!({
        "result": result,
        "albums_without_cover": still_missing,
    }))
}

fn now_iso_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date from days since epoch
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let ylen = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if d < ylen { break; }
        d -= ylen;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    for (i, &ml) in mdays.iter().enumerate() {
        if d < ml as i64 { mo = i; break; }
        d -= ml as i64;
    }
    format!("{y:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z", mo + 1, d + 1)
}

pub(crate) fn artwork_cache_dir() -> std::path::PathBuf {
    let dir = std::env::var("TUNE_ARTWORK_DIR")
        .unwrap_or_else(|_| "artwork_cache".into());
    std::path::PathBuf::from(dir)
}

// --- Duplicate detection ---

async fn list_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let (hash_dups, meta_dups) = {
        let conn = state.db.connection().lock().unwrap();

        // Duplicates by audio_hash
        let hash_dups: Vec<Value> = {
            let mut stmt = conn
                .prepare(
                    "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.audio_hash, t1.duration_ms,
                            t2.id, t2.file_path, ar2.name
                     FROM tracks t1
                     JOIN tracks t2 ON t1.audio_hash = t2.audio_hash AND t1.id < t2.id
                     LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
                     LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
                     WHERE t1.audio_hash IS NOT NULL AND t1.audio_hash != ''
                     LIMIT ? OFFSET ?",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0).unwrap_or(0),
                    "title": row.get::<_, String>(1).unwrap_or_default(),
                    "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                    "file_path": row.get::<_, Option<String>>(3).unwrap_or(None),
                    "audio_hash": row.get::<_, Option<String>>(4).unwrap_or(None),
                    "duration_ms": row.get::<_, i64>(5).unwrap_or(0),
                    "dup_id": row.get::<_, i64>(6).unwrap_or(0),
                    "dup_path": row.get::<_, Option<String>>(7).unwrap_or(None),
                    "dup_artist_name": row.get::<_, Option<String>>(8).unwrap_or(None),
                    "match_type": "audio_hash",
                }))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        };

        // Duplicates by (title + artist_name + duration_ms) where no hash match
        let meta_dups: Vec<Value> = {
            let mut stmt = conn
                .prepare(
                    "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms,
                            t2.id, t2.file_path, ar2.name
                     FROM tracks t1
                     JOIN tracks t2 ON t1.title = t2.title COLLATE NOCASE
                                   AND t1.duration_ms = t2.duration_ms
                                   AND t1.id < t2.id
                     LEFT JOIN artists ar1 ON t1.artist_id = ar1.id
                     LEFT JOIN artists ar2 ON t2.artist_id = ar2.id
                     WHERE ar1.name = ar2.name COLLATE NOCASE
                       AND (t1.audio_hash IS NULL OR t2.audio_hash IS NULL OR t1.audio_hash != t2.audio_hash)
                     LIMIT ? OFFSET ?",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![limit, offset], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0).unwrap_or(0),
                    "title": row.get::<_, String>(1).unwrap_or_default(),
                    "artist_name": row.get::<_, Option<String>>(2).unwrap_or(None),
                    "file_path": row.get::<_, Option<String>>(3).unwrap_or(None),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "dup_id": row.get::<_, i64>(5).unwrap_or(0),
                    "dup_path": row.get::<_, Option<String>>(6).unwrap_or(None),
                    "dup_artist_name": row.get::<_, Option<String>>(7).unwrap_or(None),
                    "match_type": "metadata",
                }))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        };

        (hash_dups, meta_dups)
    };

    Json(json!({
        "duplicates": {
            "by_hash": hash_dups,
            "by_metadata": meta_dups,
        },
        "total": hash_dups.len() + meta_dups.len(),
    }))
}

#[derive(Deserialize)]
struct ResolveDuplicate {
    keep_id: i64,
    delete_id: i64,
}

async fn resolve_duplicate(
    State(state): State<AppState>,
    Json(body): Json<ResolveDuplicate>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();

    // Verify both tracks exist
    let keep_exists: bool = conn
        .query_row("SELECT COUNT(*) FROM tracks WHERE id = ?", [body.keep_id], |row| row.get::<_, i64>(0))
        .unwrap_or(0) > 0;
    let delete_exists: bool = conn
        .query_row("SELECT COUNT(*) FROM tracks WHERE id = ?", [body.delete_id], |row| row.get::<_, i64>(0))
        .unwrap_or(0) > 0;

    if !keep_exists || !delete_exists {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "track not found"}))).into_response();
    }

    // Reassign playlist references from deleted track to kept track
    conn.execute(
        "UPDATE playlist_tracks SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    ).ok();

    // Reassign play queue references
    conn.execute(
        "UPDATE play_queue SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    ).ok();

    // Reassign listen history references
    conn.execute(
        "UPDATE listen_history SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    ).ok();

    // Reassign bookmarks
    conn.execute(
        "UPDATE bookmarks SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    ).ok();

    // Reassign favorites
    conn.execute(
        "UPDATE favorites SET item_id = ? WHERE item_type = 'track' AND item_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    ).ok();

    // Delete the duplicate track
    conn.execute("DELETE FROM tracks WHERE id = ?", [body.delete_id]).ok();
    drop(conn);

    Json(json!({
        "kept": body.keep_id,
        "deleted": body.delete_id,
    })).into_response()
}

// --- New routes ---

async fn library_activity(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}

async fn album_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::new(state.db.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Resolve artist MBID for the API call
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::new(state.db);
        artist_repo.get(aid).ok().flatten().and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(json!({"album": album.title, "bio": null, "error": "no artist MusicBrainz ID"})).into_response();
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

async fn album_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::new(state.db.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::new(state.db);
        artist_repo.get(aid).ok().flatten().and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(json!({"album": album.title, "artists": []})).into_response();
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

async fn rescan_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::new(state.db.clone());
    let album_repo = AlbumRepo::new(state.db);
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if tracks.is_empty() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no tracks in album"}))).into_response();
    }
    let cache_dir = artwork_cache_dir();
    let mut found_hash: Option<String> = None;
    for track in &tracks {
        if let Some(ref file_path) = track.file_path {
            if let Some(hash) = tune_core::artwork::get_or_extract(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                found_hash = Some(hash);
                break;
            }
        }
    }
    if let Some(ref hash) = found_hash {
        album_repo.update_cover_path(id, hash).ok();
    }
    Json(json!({
        "album_id": id,
        "rescanned_tracks": tracks.len(),
        "artwork_found": found_hash.is_some(),
        "hash": found_hash,
    })).into_response()
}

async fn merge_duplicate_albums_route(
    State(state): State<AppState>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let dupes: Vec<(String, String)> = conn
        .prepare("SELECT title, GROUP_CONCAT(id) FROM albums GROUP BY title HAVING COUNT(id) > 1")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 { continue; }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        for &aid in &ids {
            let cnt: i64 = conn.query_row(
                "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                rusqlite::params![aid], |r| r.get(0),
            ).unwrap_or(0);
            if cnt > best_count { best_count = cnt; best_id = aid; }
        }
        for &aid in &ids {
            if aid != best_id {
                conn.execute("UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    rusqlite::params![best_id, aid]).ok();
                conn.execute("DELETE FROM albums WHERE id = ?", rusqlite::params![aid]).ok();
                deleted += 1;
            }
        }
    }
    conn.execute_batch(
        "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
    ).ok();
    drop(conn);
    Json(json!({ "merged": deleted }))
}

async fn artist_timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db.clone());
    let artist_repo = ArtistRepo::new(state.db);
    let artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut albums = repo.list_by_artist(id).unwrap_or_default();
    albums.sort_by(|a, b| a.year.unwrap_or(0).cmp(&b.year.unwrap_or(0)));
    let items: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!({
        "artist": artist.name,
        "artist_id": id,
        "albums": items,
    })).into_response()
}

async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let artist_repo = ArtistRepo::new(state.db);
    let mut artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return (StatusCode::NOT_FOUND, Json(json!({"error": "artist not found"}))).into_response(),
    };
    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") { ext = "png".to_string(); }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }
    let Some(data) = image_data else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no image provided"}))).into_response();
    };
    let cache_dir = artwork_cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    let hash = tune_core::artwork::artwork_hash(&format!("artist-{id}"));
    let path = cache_dir.join(format!("{hash}.{ext}"));
    if std::fs::write(&path, &data).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "failed to save image"}))).into_response();
    }
    artist.image_path = Some(hash.clone());
    artist.image_source = Some("upload".into());
    artist_repo.update(&artist).ok();
    Json(json!({
        "artist_id": id,
        "hash": hash,
        "size": data.len(),
    })).into_response()
}

#[derive(Deserialize)]
struct ImageReportBody {
    reason: Option<String>,
}

async fn artist_image_report(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ImageReportBody>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let key = format!("reported_artist_image_{id}");
    let val = json!({
        "artist_id": id,
        "reason": body.reason,
        "reported_at": now_iso_utc(),
    });
    settings.set(&key, &val.to_string()).ok();
    Json(json!({"reported": true, "artist_id": id}))
}

async fn track_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let genius_token = settings.get("genius_api_token").ok().flatten();
    let Some(token) = genius_token else {
        return Json(json!({"track_id": id, "lyrics": null, "error": "Genius API not configured"})).into_response();
    };
    let title = &track.title;
    let artist = track.artist_name.as_deref().unwrap_or("");
    let q = format!("{title} {artist}");
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap();
    let search_url = format!("https://api.genius.com/search?q={}", urlencoding::encode(&q));
    let resp = client.get(&search_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let data: Value = r.json().await.unwrap_or(json!({}));
            let hits = data.pointer("/response/hits").and_then(|v| v.as_array());
            let url = hits
                .and_then(|arr| arr.first())
                .and_then(|h| h.pointer("/result/url"))
                .and_then(|v| v.as_str());
            Json(json!({
                "track_id": id,
                "title": title,
                "artist": artist,
                "genius_url": url,
                "lyrics": null,
            })).into_response()
        }
        _ => Json(json!({"track_id": id, "lyrics": null, "error": "Genius API request failed"})).into_response(),
    }
}

#[derive(Deserialize)]
struct ExportRatingQuery {
    profile_id: Option<i64>,
}

async fn export_ratings(
    State(state): State<AppState>,
    Query(q): Query<ExportRatingQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or(1);
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT r.album_id, a.title, ar.name, r.rating, r.note, r.created_at \
             FROM album_ratings r \
             LEFT JOIN albums a ON r.album_id = a.id \
             LEFT JOIN artists ar ON a.artist_id = ar.id \
             WHERE r.profile_id = ? \
             ORDER BY r.rating DESC"
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![profile_id], |row| {
                Ok(json!({
                    "album_id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "rating": row.get::<_, Option<i32>>(3).ok().flatten(),
                    "note": row.get::<_, Option<String>>(4).ok().flatten(),
                    "created_at": row.get::<_, Option<String>>(5).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!({
        "profile_id": profile_id,
        "ratings": items,
        "count": items.len(),
    }))
}

#[derive(Deserialize)]
struct ImportRatingItem {
    album_id: i64,
    rating: i32,
    note: Option<String>,
}

#[derive(Deserialize)]
struct ImportRatingsBody {
    profile_id: Option<i64>,
    ratings: Vec<ImportRatingItem>,
}

async fn import_ratings(
    State(state): State<AppState>,
    Json(body): Json<ImportRatingsBody>,
) -> Json<Value> {
    let profile_id = body.profile_id.unwrap_or(1);
    let repo = RatingRepo::new(state.db);
    let mut imported = 0i32;
    let mut failed = 0i32;
    for item in &body.ratings {
        match repo.rate_album(item.album_id, profile_id, item.rating, item.note.as_deref()) {
            Ok(_) => imported += 1,
            Err(_) => failed += 1,
        }
    }
    Json(json!({
        "imported": imported,
        "failed": failed,
        "total": body.ratings.len(),
    }))
}

async fn enrich_all_library(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let db = state.db.clone();

    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    settings.set("enrich_all_status", &json!({"status": "running", "task_id": task_id, "enriched": 0}).to_string()).ok();

    let db2 = db.clone();
    let task_id_clone = task_id.clone();
    tokio::spawn(async move {
        let track_ids: Vec<(i64, Option<String>)> = {
            let conn = db2.connection().lock().unwrap();
            conn.prepare("SELECT id, file_path FROM tracks WHERE (musicbrainz_recording_id IS NULL OR musicbrainz_recording_id = '') AND file_path IS NOT NULL")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
        };

        let total = track_ids.len();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(MB_USER_AGENT)
            .build()
            .unwrap();

        let mut enriched = 0i32;
        for (track_id, file_path) in &track_ids {
            if let Some(fp) = file_path {
                let meta = tune_core::metadata::read_metadata(std::path::Path::new(fp));
                if let Some(m) = meta {
                    if let Some(ref mbid) = m.musicbrainz_recording_id {
                        db2.execute(
                            "UPDATE tracks SET musicbrainz_recording_id = ? WHERE id = ?",
                            &[mbid as &dyn rusqlite::types::ToSql, track_id],
                        ).ok();
                        enriched += 1;
                    } else {
                        // Try MusicBrainz lookup by title+artist
                        let title = m.title.as_deref().unwrap_or("");
                        let artist = m.artist.as_deref().unwrap_or("");
                        if !title.is_empty() && !artist.is_empty() {
                            let url = format!(
                                "https://musicbrainz.org/ws/2/recording/?query=recording:{}&artist:{}&fmt=json&limit=1",
                                urlencoding::encode(title),
                                urlencoding::encode(artist),
                            );
                            if let Ok(r) = client.get(&url).send().await {
                                if r.status().is_success() {
                                    if let Ok(data) = r.json::<Value>().await {
                                        if let Some(mbid) = data.pointer("/recordings/0/id").and_then(|v| v.as_str()) {
                                            db2.execute(
                                                "UPDATE tracks SET musicbrainz_recording_id = ? WHERE id = ?",
                                                &[&mbid as &dyn rusqlite::types::ToSql, track_id],
                                            ).ok();
                                            enriched += 1;
                                        }
                                    }
                                }
                            }
                            // MusicBrainz rate limit
                            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                        }
                    }
                }
            }

            // Update status periodically
            if enriched % 50 == 0 {
                let settings = tune_core::db::settings_repo::SettingsRepo::new(db2.clone());
                settings.set("enrich_all_status", &json!({
                    "status": "running",
                    "task_id": task_id_clone,
                    "enriched": enriched,
                    "total": total,
                }).to_string()).ok();
            }
        }

        let settings = tune_core::db::settings_repo::SettingsRepo::new(db2);
        settings.set("enrich_all_status", &json!({
            "status": "done",
            "task_id": task_id_clone,
            "enriched": enriched,
            "total": total,
        }).to_string()).ok();
        tracing::info!(task_id = %task_id_clone, enriched, total, "enrich_all_library done");
    });

    (StatusCode::ACCEPTED, Json(json!({"status": "accepted", "task_id": task_id})))
}

async fn enrich_all_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let result = settings
        .get("enrich_all_status")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}));
    Json(result)
}

async fn rescan_all_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.db.clone();

    tokio::spawn(async move {
        let albums: Vec<(i64,)> = {
            let conn = db.connection().lock().unwrap();
            conn.prepare("SELECT id FROM albums")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?,)))
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                })
                .unwrap_or_default()
        };

        let track_repo = TrackRepo::new(db.clone());
        let album_repo = AlbumRepo::new(db);
        let mut updated = 0i32;
        for (album_id,) in &albums {
            let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();
            for track in &tracks {
                if let Some(ref file_path) = track.file_path {
                    if let Some(hash) = tune_core::artwork::get_or_extract(
                        std::path::Path::new(file_path),
                        &cache_dir,
                    ) {
                        album_repo.update_cover_path(*album_id, &hash).ok();
                        updated += 1;
                        break;
                    }
                }
            }
        }
        tracing::info!(updated, total = albums.len(), "rescan_all_artwork done");
    });

    (StatusCode::ACCEPTED, Json(json!({"status": "accepted", "message": "artwork rescan started"})))
}

async fn smart_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let conn = state.db.connection().lock().unwrap();
    // Find tracks with same title (case-insensitive), same artist, and similar duration (within 3 seconds)
    let items: Vec<Value> = conn
        .prepare(
            "SELECT t1.id, t1.title, ar1.name, t1.file_path, t1.duration_ms, t1.format, t1.sample_rate, t1.bit_depth, \
                    t2.id, t2.file_path, t2.duration_ms, t2.format, t2.sample_rate, t2.bit_depth, ar2.name \
             FROM tracks t1 \
             JOIN tracks t2 ON t1.title = t2.title COLLATE NOCASE AND t1.id < t2.id \
             LEFT JOIN artists ar1 ON t1.artist_id = ar1.id \
             LEFT JOIN artists ar2 ON t2.artist_id = ar2.id \
             WHERE ar1.name = ar2.name COLLATE NOCASE \
               AND ABS(COALESCE(t1.duration_ms,0) - COALESCE(t2.duration_ms,0)) < 3000 \
             LIMIT ? OFFSET ?"
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![limit, offset], |row| {
                Ok(json!({
                    "track_a": {
                        "id": row.get::<_, i64>(0).unwrap_or(0),
                        "title": row.get::<_, String>(1).unwrap_or_default(),
                        "artist": row.get::<_, Option<String>>(2).unwrap_or(None),
                        "file_path": row.get::<_, Option<String>>(3).unwrap_or(None),
                        "duration_ms": row.get::<_, Option<i64>>(4).unwrap_or(None),
                        "format": row.get::<_, Option<String>>(5).unwrap_or(None),
                        "sample_rate": row.get::<_, Option<i32>>(6).unwrap_or(None),
                        "bit_depth": row.get::<_, Option<i32>>(7).unwrap_or(None),
                    },
                    "track_b": {
                        "id": row.get::<_, i64>(8).unwrap_or(0),
                        "file_path": row.get::<_, Option<String>>(9).unwrap_or(None),
                        "duration_ms": row.get::<_, Option<i64>>(10).unwrap_or(None),
                        "format": row.get::<_, Option<String>>(11).unwrap_or(None),
                        "sample_rate": row.get::<_, Option<i32>>(12).unwrap_or(None),
                        "bit_depth": row.get::<_, Option<i32>>(13).unwrap_or(None),
                        "artist": row.get::<_, Option<String>>(14).unwrap_or(None),
                    },
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    Json(json!({
        "duplicates": items,
        "count": items.len(),
    }))
}

// --- Collections CRUD ---

#[derive(Deserialize)]
struct CreateCollectionBody {
    name: String,
    description: Option<String>,
}

async fn list_collections(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let data = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();
    Json(json!(data))
}

async fn create_collection(
    State(state): State<AppState>,
    Json(body): Json<CreateCollectionBody>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = collections.iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(0) + 1;

    let collection = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "album_ids": [],
        "created_at": now_iso_utc(),
    });
    collections.push(collection.clone());
    settings.set("collections", &serde_json::to_string(&collections).unwrap()).ok();

    (StatusCode::CREATED, Json(collection)).into_response()
}

async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections.iter().find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    match found {
        Some(c) => Json(c.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = collections.len();
    collections.retain(|c| c.get("id").and_then(|v| v.as_i64()) != Some(id));
    if collections.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    settings.set("collections", &serde_json::to_string(&collections).unwrap()).ok();
    StatusCode::NO_CONTENT.into_response()
}

async fn collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections.iter().find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let album_ids: Vec<i64> = collection.get("album_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let album_repo = AlbumRepo::new(state.db);
    let albums: Vec<Value> = album_ids.iter()
        .filter_map(|&aid| album_repo.get(aid).ok().flatten().map(|a| a.to_json()))
        .collect();
    Json(json!(albums)).into_response()
}

#[derive(Deserialize)]
struct CollectionAlbumPath {
    id: i64,
    album_id: i64,
}

async fn add_album_to_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections.iter_mut().find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let album_ids = collection.get_mut("album_ids")
        .and_then(|v| v.as_array_mut());
    match album_ids {
        Some(arr) => {
            let already = arr.iter().any(|v| v.as_i64() == Some(path.album_id));
            if !already {
                arr.push(json!(path.album_id));
            }
        }
        None => {
            collection.as_object_mut().unwrap().insert("album_ids".into(), json!([path.album_id]));
        }
    }
    settings.set("collections", &serde_json::to_string(&collections).unwrap()).ok();
    Json(json!({"added": true, "collection_id": path.id, "album_id": path.album_id})).into_response()
}

async fn remove_album_from_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let mut collections: Vec<Value> = settings.get("collections").ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections.iter_mut().find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(arr) = collection.get_mut("album_ids").and_then(|v| v.as_array_mut()) {
        arr.retain(|v| v.as_i64() != Some(path.album_id));
    }
    settings.set("collections", &serde_json::to_string(&collections).unwrap()).ok();
    Json(json!({"removed": true, "collection_id": path.id, "album_id": path.album_id})).into_response()
}
