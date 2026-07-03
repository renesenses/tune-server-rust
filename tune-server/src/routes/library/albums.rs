use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::models::Album;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::rating_repo::RatingRepo;
use tune_core::db::track_repo::TrackRepo;

use super::Pagination;

#[derive(Deserialize)]
pub(super) struct AlbumFilters {
    limit: Option<i64>,
    offset: Option<i64>,
    quality: Option<String>,
    format: Option<String>,
    sort: Option<String>,
    order: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RateRequest {
    rating: i32,
    note: Option<String>,
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RatingQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

pub(super) async fn list_albums(
    State(state): State<AppState>,
    Query(p): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let sort = p.sort.as_deref().unwrap_or("added_at");
    let order = p.order.as_deref().unwrap_or("asc");
    let total = repo.count().unwrap_or(0);
    let items = match repo.list_filtered(
        limit,
        offset,
        sort,
        order,
        p.format.as_deref(),
        p.quality.as_deref(),
    ) {
        Ok(albums) => albums,
        Err(e) => {
            tracing::error!(
                error = %e,
                sort,
                order,
                limit,
                offset,
                total,
                "list_albums_query_failed — stats show {total} albums but query returned error"
            );
            Vec::new()
        }
    };
    let items: Vec<Value> = items
        .iter()
        .map(|a| {
            let mut j = a.to_json();
            if let Some(obj) = j.as_object_mut() {
                obj.remove("bio");
            }
            j
        })
        .collect();
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn album_count(State(state): State<AppState>) -> Json<Value> {
    let count = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

#[derive(Deserialize)]
pub(super) struct CreateAlbumRequest {
    title: String,
    artist_id: Option<i64>,
}

/// Create an album by title (used by MetadataView when assigning tracks to a
/// new album name). Reuses an existing album with the same title if one exists.
pub(super) async fn create_album(
    State(state): State<AppState>,
    Json(body): Json<CreateAlbumRequest>,
) -> Result<impl IntoResponse, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request("title is required"));
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    if let Ok(Some(existing)) = repo.get_by_title(title) {
        return Ok(Json(json!({ "id": existing.id, "title": existing.title })));
    }
    let album = Album {
        id: None,
        title: title.to_string(),
        artist_id: body.artist_id,
        artist_name: None,
        year: None,
        original_year: None,
        genre: None,
        genres: None,
        disc_count: None,
        track_count: None,
        cover_path: None,
        source: "local".to_string(),
        source_id: None,
        label: None,
        catalog_number: None,
        barcode: None,
        format: None,
        sample_rate: None,
        bit_depth: None,
        bio: None,
        musicbrainz_release_id: None,
        musicbrainz_release_group_id: None,
        release_date: None,
        original_date: None,
    };
    let id = repo
        .create(&album)
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!({ "id": id, "title": title })))
}

pub(super) async fn album_filters(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let formats: Vec<String> = state
        .backend
        .query_many(
            "SELECT DISTINCT format FROM albums WHERE format IS NOT NULL ORDER BY format",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_string())
        .collect();
    let sample_rates: Vec<i64> = state
        .backend
        .query_many(
            "SELECT DISTINCT sample_rate FROM albums WHERE sample_rate IS NOT NULL ORDER BY sample_rate",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_i64())
        .collect();
    Ok(Json(
        json!({ "formats": formats, "sample_rates": sample_rates }),
    ))
}

pub(super) async fn recent_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn get_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(album)) => Json(album.to_json()).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn album_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let items = repo.list_by_album(id).unwrap_or_default();
    Json(json!(items))
}

pub(super) async fn quick_fav_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or(1);
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let is_fav = repo.is_favorite(profile_id, "album", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "album", id).ok();
    } else {
        repo.add_favorite(profile_id, "album", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "album_id": id}))
}

pub(super) async fn rate_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RateRequest>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = body.profile_id.unwrap_or(1);
    match repo.rate_album(id, profile_id, body.rating, body.note.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub(super) async fn get_album_rating(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RatingQuery>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = q.profile_id.unwrap_or(1);
    match repo.get_rating(id, profile_id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => Json(json!({ "rating": null, "album_id": id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn top_rated_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = RatingRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
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

pub(super) async fn recommendations(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    // Return recently added albums the user hasn't listened to
    let limit = p.limit.unwrap_or(20);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!({ "albums": items }))
}

pub(super) async fn album_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    // Resolve artist MBID for the API call
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        artist_repo
            .get(aid)
            .ok()
            .flatten()
            .and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(
            json!({"album": album.title, "bio": null, "error": "no artist MusicBrainz ID"}),
        )
        .into_response();
    };
    let lang = q.lang.as_deref().unwrap_or("fr");
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/bio?lang={lang}"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "bio": null})).into_response(),
    }
}

pub(super) async fn album_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        artist_repo
            .get(aid)
            .ok()
            .flatten()
            .and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(json!({"album": album.title, "artists": []})).into_response();
    };
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/similar"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

pub(super) async fn merge_duplicate_albums_route(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Pick engine-specific aggregate and placeholder helpers.
    let (group_concat_expr, p1, p2) = match state.backend.engine() {
        Engine::Postgres => (
            PostgresDialect.group_concat(&PostgresDialect.placeholder(1), ","),
            PostgresDialect.placeholder(1),
            PostgresDialect.placeholder(2),
        ),
        Engine::Sqlite => (
            SqliteDialect.group_concat("id", ","),
            SqliteDialect.placeholder(1),
            SqliteDialect.placeholder(2),
        ),
    };

    // Case-insensitive grouping: LOWER(title) catches duplicates that differ
    // only by case (e.g. "The Dark Side of the Moon" vs "The Dark Side Of The Moon").
    let dupes_sql = format!(
        "SELECT LOWER(title), {group_concat_expr} FROM albums WHERE source = 'local' GROUP BY LOWER(title) HAVING COUNT(id) > 1"
    );
    let dupes: Vec<(String, String)> = state
        .backend
        .query_many(&dupes_sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let title = row.first()?.as_string()?;
            let ids = row.get(1)?.as_string()?;
            Some((title, ids))
        })
        .collect();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        let count_sql = format!("SELECT COUNT(id) FROM tracks WHERE album_id = {p1}");
        for &aid in &ids {
            let cnt: i64 = state
                .backend
                .query_one(&count_sql, &[&aid as &dyn ToSqlValue])
                .ok()
                .flatten()
                .and_then(|row| row.into_iter().next()?.as_i64())
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        let update_sql = format!("UPDATE tracks SET album_id = {p1} WHERE album_id = {p2}");
        let delete_sql = format!("DELETE FROM albums WHERE id = {p1}");
        for &aid in &ids {
            if aid != best_id {
                state
                    .backend
                    .execute(
                        &update_sql,
                        &[&best_id as &dyn ToSqlValue, &aid as &dyn ToSqlValue],
                    )
                    .ok();
                state
                    .backend
                    .execute(&delete_sql, &[&aid as &dyn ToSqlValue])
                    .ok();
                deleted += 1;
            }
        }
    }
    state
        .backend
        .execute_batch(
            "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
        )
        .ok();
    Ok(Json(json!({ "merged": deleted })))
}

const VARIANT_PATTERNS: &[&str] = &[
    "deluxe",
    "remastered",
    "remaster",
    "anniversary",
    "expanded",
    "special edition",
    "collector",
    "bonus track",
    "super deluxe",
    "legacy edition",
    "platinum edition",
];

fn strip_variant_suffix(title: &str) -> String {
    let lower = title.to_lowercase();
    for pat in VARIANT_PATTERNS {
        if let Some(pos) = lower.find(pat) {
            let prefix = title[..pos]
                .trim_end_matches(|c: char| c == '(' || c == '[' || c == '-' || c == ' ');
            if !prefix.is_empty() {
                return prefix.to_string();
            }
        }
    }
    title.to_string()
}

pub(super) async fn albums_grouped(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());

    // Group by MusicBrainz release group ID
    let mbid_groups = repo.list_release_groups().unwrap_or_default();

    let mut groups: Vec<Value> = mbid_groups
        .iter()
        .map(|(gid, albums)| {
            let original = &albums[0];
            json!({
                "group_id": gid,
                "method": "musicbrainz",
                "original": original.to_json(),
                "variants": albums[1..].iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": albums.len(),
            })
        })
        .collect();

    // Group by title similarity (regex) for albums without MBID
    let all_albums = repo.list(5000, 0).unwrap_or_default();
    let grouped_ids: std::collections::HashSet<i64> = mbid_groups
        .iter()
        .flat_map(|(_, albums)| albums.iter().filter_map(|a| a.id))
        .collect();

    let ungrouped: Vec<_> = all_albums
        .iter()
        .filter(|a| a.id.is_some() && !grouped_ids.contains(&a.id.unwrap()))
        .collect();

    let mut title_map: std::collections::HashMap<String, Vec<&tune_core::db::models::Album>> =
        std::collections::HashMap::new();
    for album in &ungrouped {
        let base = strip_variant_suffix(&album.title);
        title_map.entry(base).or_default().push(album);
    }

    for (base_title, albums) in &title_map {
        if albums.len() > 1 {
            groups.push(json!({
                "group_id": base_title,
                "method": "title_similarity",
                "original": albums[0].to_json(),
                "variants": albums[1..].iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": albums.len(),
            }));
        }
    }

    Ok(Json(json!({
        "groups": groups,
        "total_groups": groups.len(),
    })))
}

pub(super) async fn album_completeness(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("album not found"))?;

    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };

    let actual_tracks: i64 = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE album_id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);
    let expected_tracks = album.track_count.unwrap_or(0) as i64;

    // Check total_tracks from metadata tags
    let max_tag_total: i64 = state
        .backend
        .query_one(
            &format!("SELECT COALESCE(MAX(CAST(track_number AS INTEGER)), 0) FROM tracks WHERE album_id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);

    let expected = if expected_tracks > 0 {
        expected_tracks
    } else {
        max_tag_total
    };
    let complete = expected > 0 && actual_tracks >= expected;
    let missing = if expected > actual_tracks {
        expected - actual_tracks
    } else {
        0
    };

    Ok(Json(json!({
        "album_id": id,
        "album_title": album.title,
        "actual_tracks": actual_tracks,
        "expected_tracks": expected,
        "missing": missing,
        "complete": complete,
        "completeness_pct": if expected > 0 { (actual_tracks as f64 / expected as f64 * 100.0).round() } else { 100.0 },
    })))
}

// ---------------------------------------------------------------------------
// PUT /albums/{id} — update album metadata (mirrors POST /metadata/albums/{id}/edit)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct AlbumUpdate {
    title: Option<String>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    label: Option<String>,
}

pub(super) async fn update_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.title {
        album.title = v.clone();
    }
    if let Some(ref v) = body.genre {
        album.genre = Some(v.clone());
    }
    if let Some(v) = body.year {
        album.year = Some(v);
    }
    if let Some(ref v) = body.label {
        album.label = Some(v.clone());
    }
    // artist_id takes priority; fall back to artist_name resolution
    if let Some(aid) = body.artist_id {
        album.artist_id = Some(aid);
        // Refresh artist_name for the JSON response
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        album.artist_name = artist_repo.get(aid).ok().flatten().map(|a| a.name);
    } else if let Some(ref name) = body.artist_name {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        if let Ok(Some(artist)) = artist_repo.get_by_name(name) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        } else if let Ok(artist) = artist_repo.get_or_create(name, None, None) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        }
    }

    repo.update(&album).ok();

    Json(album.to_json()).into_response()
}

#[derive(Deserialize)]
pub(super) struct BatchAlbumUpdate {
    album_ids: Vec<i64>,
    genre: Option<String>,
    year: Option<i32>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    label: Option<String>,
}

pub(super) async fn batch_update_albums(
    State(state): State<AppState>,
    Json(body): Json<BatchAlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let mut updated = 0i64;

    let resolved_artist_id = if let Some(aid) = body.artist_id {
        Some(aid)
    } else if let Some(ref name) = body.artist_name {
        artist_repo
            .get_by_name(name)
            .ok()
            .flatten()
            .and_then(|a| a.id)
            .or_else(|| {
                artist_repo
                    .get_or_create(name, None, None)
                    .ok()
                    .and_then(|a| a.id)
            })
    } else {
        None
    };

    for &id in &body.album_ids {
        let mut album = match repo.get(id) {
            Ok(Some(a)) => a,
            _ => continue,
        };
        if let Some(ref g) = body.genre {
            album.genre = Some(g.clone());
        }
        if let Some(y) = body.year {
            album.year = Some(y);
        }
        if let Some(ref l) = body.label {
            album.label = Some(l.clone());
        }
        if let Some(aid) = resolved_artist_id {
            album.artist_id = Some(aid);
        }
        if repo.update(&album).is_ok() {
            updated += 1;
        }
    }

    Json(serde_json::json!({ "updated": updated, "total": body.album_ids.len() })).into_response()
}
