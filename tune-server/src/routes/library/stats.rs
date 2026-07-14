use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::history_repo::HistoryRepo;

use super::Pagination;

pub(super) async fn library_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let b = &state.backend;
    let (dur_col, size_col) = match b.engine() {
        tune_core::db::engine::Engine::Sqlite => ("duration_ms", "file_size"),
        tune_core::db::engine::Engine::Postgres => {
            ("CAST(duration_ms AS bigint)", "CAST(file_size AS bigint)")
        }
    };
    let sql = format!(
        "SELECT \
         (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
         (SELECT COUNT(*) FROM albums), \
         (SELECT COUNT(*) FROM tracks), \
         (SELECT COUNT(*) FROM zones), \
         COALESCE(CAST((SELECT SUM({dur_col}) FROM tracks) AS bigint), 0), \
         COALESCE(CAST((SELECT SUM({size_col}) FROM tracks WHERE file_size IS NOT NULL) AS bigint), 0)"
    );
    let row = b
        .query_one(&sql, &[])
        .map_err(|e| AppError::internal(e))?
        .unwrap_or_default();

    let artists = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let albums = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let tracks = row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
    let zones = row.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_duration_ms = row.get(4).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_size_bytes = row.get(5).and_then(|v| v.as_i64()).unwrap_or(0);

    let listens = b
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

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

pub(super) async fn completeness_stats(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
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
    let albums_with_genre: i64 = conn.query_row("SELECT COUNT(DISTINCT a.id) FROM albums a JOIN tracks t ON t.album_id = a.id WHERE t.genre IS NOT NULL AND t.genre != ''", [], |row| row.get(0)).unwrap_or(0);
    let albums_with_year: i64 = conn.query_row("SELECT COUNT(DISTINCT a.id) FROM albums a JOIN tracks t ON t.album_id = a.id WHERE t.year IS NOT NULL AND t.year > 0", [], |row| row.get(0)).unwrap_or(0);
    drop(conn);

    // Count only album-artists — the same set the library shows — so the
    // completeness figures match the artist total elsewhere. Counting every row
    // in `artists` over-counted by ~the number of compilation/track-only artists
    // (Bilou: 1808 vs 1505 real artists).
    let (total_artists, artists_without_image): (i64, i64) = {
        let conn = state
            .db
            .connection()
            .lock()
            .map_err(|e| AppError::internal(format!("{e}")))?;
        let total = conn
            .query_row(
                "SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        // Real "without image" count over the same album-artist set — was
        // previously just `total_artists`, i.e. every artist reported as missing.
        let without_image = conn
            .query_row(
                "SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL) AND (image_path IS NULL OR image_path = '')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        (total, without_image)
    };

    let genre_pct = if total_tracks > 0 {
        with_genre as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let year_pct = if total_tracks > 0 {
        with_year as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let artist_pct = if total_tracks > 0 {
        with_artist as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };
    let cover_pct = if total_albums > 0 {
        with_cover as f64 / total_albums as f64 * 100.0
    } else {
        0.0
    };
    let mbid_pct = if total_tracks > 0 {
        with_mbid as f64 / total_tracks as f64 * 100.0
    } else {
        0.0
    };

    // Weighted health score: cover(30%) + genre(25%) + year(20%) + mbid(15%) + artist(10%)
    let health_score = (cover_pct * 0.30
        + genre_pct * 0.25
        + year_pct * 0.20
        + mbid_pct * 0.15
        + artist_pct * 0.10)
        .round();

    let grade = match health_score as u32 {
        90..=100 => "A",
        75..=89 => "B",
        50..=74 => "C",
        25..=49 => "D",
        _ => "F",
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
        "albums_without_genre": total_albums - albums_with_genre,
        "albums_without_year": total_albums - albums_with_year,
        "tracks_without_artist": total_tracks - with_artist,
        "artists_without_image": artists_without_image,
        "genre_pct": genre_pct.round(),
        "year_pct": year_pct.round(),
        "artist_pct": artist_pct.round(),
        "album_pct": if total_tracks > 0 { (with_album as f64 / total_tracks as f64 * 100.0).round() } else { 0.0 },
        "cover_pct": cover_pct.round(),
        "mbid_pct": mbid_pct.round(),
        "health_score": health_score,
        "health_grade": grade,
    })))
}

pub(super) async fn library_activity(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let items = repo.recent(limit).unwrap_or_default();
    Json(json!(items))
}
