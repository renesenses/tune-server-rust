use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::AppState;

use super::Pagination;

#[derive(Deserialize)]
pub(super) struct ResolveDuplicate {
    keep_id: i64,
    delete_id: i64,
}

pub(super) async fn list_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let (hash_dups, meta_dups) = {
        let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;

        // Duplicates by audio_hash
        let hash_dups: Vec<Value> = conn
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
            .and_then(|mut stmt| {
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
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();

        // Duplicates by (title + artist_name + duration_ms) where no hash match
        let meta_dups: Vec<Value> = conn
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
            .and_then(|mut stmt| {
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
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();

        (hash_dups, meta_dups)
    };

    Ok(Json(json!({
        "duplicates": {
            "by_hash": hash_dups,
            "by_metadata": meta_dups,
        },
        "total": hash_dups.len() + meta_dups.len(),
    })))
}

pub(super) async fn resolve_duplicate(
    State(state): State<AppState>,
    Json(body): Json<ResolveDuplicate>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;

    // Verify both tracks exist
    let keep_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE id = ?",
            [body.keep_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    let delete_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE id = ?",
            [body.delete_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !keep_exists || !delete_exists {
        return Err(AppError::not_found("track not found"));
    }

    // Reassign playlist references from deleted track to kept track
    conn.execute(
        "UPDATE playlist_tracks SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    )
    .ok();

    // Reassign play queue references
    conn.execute(
        "UPDATE play_queue SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    )
    .ok();

    // Reassign listen history references
    conn.execute(
        "UPDATE listen_history SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    )
    .ok();

    // Reassign bookmarks
    conn.execute(
        "UPDATE bookmarks SET track_id = ? WHERE track_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    )
    .ok();

    // Reassign favorites
    conn.execute(
        "UPDATE favorites SET item_id = ? WHERE item_type = 'track' AND item_id = ?",
        rusqlite::params![body.keep_id, body.delete_id],
    )
    .ok();

    // Delete the duplicate track
    conn.execute("DELETE FROM tracks WHERE id = ?", [body.delete_id])
        .ok();
    drop(conn);

    Ok(Json(json!({
        "kept": body.keep_id,
        "deleted": body.delete_id,
    })))
}

pub(super) async fn smart_duplicates(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Result<Json<Value>, AppError> {
    let limit = p.limit.unwrap_or(100);
    let offset = p.offset.unwrap_or(0);

    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
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
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    Ok(Json(json!({
        "duplicates": items,
        "count": items.len(),
    })))
}
