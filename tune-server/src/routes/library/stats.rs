use axum::extract::{Query, State};
use axum::Json;
use serde_json::{json, Value};

use tune_core::db::history_repo::HistoryRepo;
use crate::error::AppError;
use crate::state::AppState;

use super::Pagination;

pub(super) async fn library_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
    let (artists, albums, tracks, zones, total_duration_ms, total_size_bytes): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT \
             (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
             (SELECT COUNT(*) FROM albums), \
             (SELECT COUNT(*) FROM tracks), \
             (SELECT COUNT(*) FROM zones), \
             COALESCE((SELECT SUM(duration_ms) FROM tracks), 0), \
             COALESCE((SELECT SUM(file_size) FROM tracks WHERE file_size IS NOT NULL), 0)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
        .unwrap_or((0, 0, 0, 0, 0, 0));
    let listens: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM listen_history",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    Ok(Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "zones": zones,
        "total_duration_ms": total_duration_ms,
        "total_size_bytes": total_size_bytes,
    })))
}

pub(super) async fn completeness_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
    let total_tracks: i64 = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
        .unwrap_or(0);
    let with_genre: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE genre IS NOT NULL AND genre != ''",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let with_year: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE year IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let with_artist: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE artist_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let with_album: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE album_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let with_cover: i64 = conn.query_row("SELECT COUNT(DISTINCT a.id) FROM albums a WHERE a.cover_path IS NOT NULL AND a.cover_path != ''", [], |row| row.get(0)).unwrap_or(0);
    let total_albums: i64 = conn
        .query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))
        .unwrap_or(0);
    let with_mbid: i64 = conn.query_row("SELECT COUNT(*) FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''", [], |row| row.get(0)).unwrap_or(0);
    drop(conn);

    let total_artists: i64 = {
        let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
        conn.query_row("SELECT COUNT(*) FROM artists", [], |row| row.get(0))
            .unwrap_or(0)
    };

    Ok(Json(json!({
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
    })))
}

pub(super) async fn library_activity(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::new(state.db);
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}
