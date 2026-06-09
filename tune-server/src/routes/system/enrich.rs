use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

pub(super) async fn system_enrich(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.clone();
    let cache_dir = crate::routes::library::artwork_cache_dir();
    let artist_db = state.db.clone();
    let artist_cache_dir = cache_dir.clone();
    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });
    let mbid_db = state.db.clone();
    let art_db = artist_db.clone();
    let art_cache = artist_cache_dir.clone();
    tokio::spawn(async move {
        // 1. Match MusicBrainz IDs for artists without one
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tune_core::metadata::matcher::batch_match_artist_mbids(mbid_db).await;
        // 2. Fetch images for artists with MBID
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::library::artwork::batch_enrich_artist_artwork(art_db, art_cache).await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "enrichment_started" })),
    )
}

pub(super) async fn cleanup(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let album_repo = AlbumRepo::new(state.db.clone());
    let artist_repo = ArtistRepo::new(state.db.clone());

    let merged_albums = merge_duplicate_albums(&state.db)?;
    let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
    let orphan_artists = artist_repo.cleanup_orphans().unwrap_or(0);
    let tracks = TrackRepo::new(state.db.clone()).deduplicate().unwrap_or(0);

    let orphan_artwork = cleanup_orphan_artwork(&state.db)?;

    let db_optimized = state.db.execute_batch("PRAGMA optimize; ANALYZE;").is_ok();

    Ok(Json(json!({
        "duplicate_albums_merged": merged_albums,
        "orphan_albums_deleted": orphan_albums,
        "orphan_artists_deleted": orphan_artists,
        "duplicate_tracks_removed": tracks,
        "orphan_artwork_deleted": orphan_artwork,
        "db_optimized": db_optimized,
    })))
}

fn merge_duplicate_albums(db: &tune_core::db::sqlite::SqliteDb) -> Result<i64, AppError> {
    let conn = db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let dupes: Vec<(String, String)> = conn
        .prepare("SELECT title, GROUP_CONCAT(id) FROM albums GROUP BY title HAVING COUNT(id) > 1")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        for &aid in &ids {
            let cnt: i64 = conn
                .query_row(
                    "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                    rusqlite::params![aid],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        for &aid in &ids {
            if aid != best_id {
                conn.execute(
                    "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    rusqlite::params![best_id, aid],
                )
                .ok();
                conn.execute("DELETE FROM albums WHERE id = ?", rusqlite::params![aid])
                    .ok();
                deleted += 1;
            }
        }
    }
    conn.execute_batch(
        "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
    ).ok();
    Ok(deleted)
}

fn cleanup_orphan_artwork(db: &tune_core::db::sqlite::SqliteDb) -> Result<i64, AppError> {
    let cache_dir = crate::routes::library::artwork_cache_dir();
    if !cache_dir.exists() {
        return Ok(0);
    }

    let conn = db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT cover_path FROM albums WHERE cover_path IS NOT NULL \
         UNION SELECT image_path FROM artists WHERE image_path IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
            for hash in rows.flatten() {
                referenced.insert(hash);
            }
        }
    }
    drop(conn);

    // Walk artwork cache and delete files whose stem (hash) isn't referenced
    let mut deleted = 0i64;
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !stem.is_empty() && !referenced.contains(stem) {
                    if std::fs::remove_file(&path).is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
    }

    if deleted > 0 {
        tracing::info!(deleted, "orphan_artwork_cleaned");
    }
    Ok(deleted)
}
