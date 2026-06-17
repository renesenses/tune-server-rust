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
    let db = state.backend.clone();
    let cache_dir = crate::routes::library::artwork_cache_dir();
    let artist_cache_dir = cache_dir.clone();
    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });
    let mbid_db = state.backend.clone();
    let art_db = state.backend.clone();
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

pub(super) async fn enrich_bios(State(state): State<AppState>) -> impl IntoResponse {
    let artist_db = state.backend.clone();
    let album_db = state.backend.clone();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let without_artist_bio = artist_repo.list_without_bio().unwrap_or_default().len();
    let without_album_bio = album_repo.list_without_bio().unwrap_or_default().len();

    tokio::spawn(async move {
        tune_core::metadata::bio_batch::batch_enrich_artist_bios(artist_db).await;
    });
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tune_core::metadata::bio_batch::batch_enrich_album_bios(album_db).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "bio_enrichment_started",
            "artists_without_bio": without_artist_bio,
            "albums_without_bio": without_album_bio,
        })),
    )
}

pub(super) async fn enrich_extended_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.backend.clone();
    tokio::spawn(async move {
        let meta_repo =
            tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
        let tracks: Vec<(i64, String)> = db
            .query_many(
                "SELECT id, file_path FROM tracks WHERE file_path IS NOT NULL AND source = 'local'",
                &[],
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|cols| {
                let id = cols.first()?.as_i64()?;
                let path = cols.get(1)?.as_string()?;
                Some((id, path))
            })
            .collect();
        let total = tracks.len();
        tracing::info!(total, "enrich_extended_metadata_started");
        let mut enriched = 0u64;
        let mut empty_count = 0u64;
        let mut not_exist_count = 0u64;
        let mut batch: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();
        let mut batch_num = 0u64;
        let mut total_fields = 0u64;
        for (track_id, file_path) in &tracks {
            let path = std::path::Path::new(file_path);
            if !path.exists() {
                not_exist_count += 1;
                continue;
            }
            let ext =
                tokio::task::block_in_place(|| tune_core::metadata::read_extended_metadata(path));
            if ext.is_empty() {
                empty_count += 1;
                // TEMP DEBUG: log first few empty results to see if it's a pattern
                if empty_count <= 3 {
                    tracing::warn!(
                        track_id,
                        file_path = %file_path,
                        "enrich_debug_read_extended_metadata_empty"
                    );
                }
            } else {
                // TEMP DEBUG: log first enriched entry details
                if enriched == 0 {
                    let keys: Vec<&str> = ext.keys().map(|k| k.as_str()).collect();
                    tracing::warn!(
                        track_id,
                        file_path = %file_path,
                        field_count = ext.len(),
                        keys = ?keys,
                        "enrich_debug_first_enriched_entry"
                    );
                }
                total_fields += ext.len() as u64;
                batch.push((*track_id, ext));
                enriched += 1;
            }
            if batch.len() >= 500 {
                batch_num += 1;
                let batch_size = batch.len();
                tracing::warn!(batch_num, batch_size, "enrich_debug_flushing_batch");
                if let Err(e) = meta_repo.set_batch_multi(&batch) {
                    tracing::error!(error = %e, batch_num, "enrich_metadata_batch_failed");
                }
                // TEMP DEBUG: verify data was actually written by reading back the first entry
                if batch_num == 1 {
                    if let Some((first_id, _)) = batch.first() {
                        match meta_repo.get_all(*first_id) {
                            Ok(readback) => {
                                tracing::warn!(
                                    track_id = first_id,
                                    readback_count = readback.len(),
                                    readback_keys = ?readback.keys().collect::<Vec<_>>(),
                                    "enrich_debug_readback_after_first_batch"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    track_id = first_id,
                                    error = %e,
                                    "enrich_debug_readback_failed"
                                );
                            }
                        }
                    }
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            batch_num += 1;
            let batch_size = batch.len();
            tracing::warn!(batch_num, batch_size, "enrich_debug_flushing_final_batch");
            if let Err(e) = meta_repo.set_batch_multi(&batch) {
                tracing::error!(error = %e, batch_num, "enrich_metadata_batch_failed");
            }
        }
        tracing::info!(
            total,
            enriched,
            empty_count,
            not_exist_count,
            total_fields,
            batch_num,
            "enrich_extended_metadata_complete"
        );
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "extended_metadata_enrichment_started"})),
    )
}

pub(super) async fn cleanup(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());

    let merged_albums = merge_duplicate_albums(&state.backend)?;
    let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
    let orphan_artists = artist_repo.cleanup_orphans().unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .deduplicate()
        .unwrap_or(0);

    let orphan_artwork = cleanup_orphan_artwork(&state.backend)?;

    let db_optimized = if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        state
            .backend
            .execute_batch("PRAGMA optimize; ANALYZE;")
            .is_ok()
    } else {
        state.backend.execute_batch("ANALYZE;").is_ok()
    };

    Ok(Json(json!({
        "duplicate_albums_merged": merged_albums,
        "orphan_albums_deleted": orphan_albums,
        "orphan_artists_deleted": orphan_artists,
        "duplicate_tracks_removed": tracks,
        "orphan_artwork_deleted": orphan_artwork,
        "db_optimized": db_optimized,
    })))
}

fn merge_duplicate_albums(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<i64, AppError> {
    // Group by (LOWER(title), artist_id) so that albums with the same title
    // but different artists are NOT merged (e.g. "One by One" by Grey Reverend
    // vs "One by One" by Robert Francis).
    let dupe_rows = db.query_many(
        "SELECT LOWER(title), GROUP_CONCAT(id) FROM albums WHERE source = 'local' GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1",
        &[],
    ).unwrap_or_default();
    let dupes: Vec<(String, String)> = dupe_rows
        .iter()
        .map(|r| {
            (
                r[0].as_string().unwrap_or_default(),
                r[1].as_string().unwrap_or_default(),
            )
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
        for &aid in &ids {
            let cnt = db
                .query_one("SELECT COUNT(id) FROM tracks WHERE album_id = ?", &[&aid])
                .ok()
                .flatten()
                .and_then(|r| r[0].as_i64())
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        for &aid in &ids {
            if aid != best_id {
                db.execute(
                    "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                    &[&best_id, &aid],
                )
                .ok();
                db.execute("DELETE FROM albums WHERE id = ?", &[&aid]).ok();
                deleted += 1;
            }
        }
    }
    db.execute_batch(
        "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
    ).ok();
    Ok(deleted)
}

fn cleanup_orphan_artwork(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<i64, AppError> {
    let cache_dir = crate::routes::library::artwork_cache_dir();
    if !cache_dir.exists() {
        return Ok(0);
    }

    let rows = db
        .query_many(
            "SELECT cover_path FROM albums WHERE cover_path IS NOT NULL \
         UNION SELECT image_path FROM artists WHERE image_path IS NOT NULL",
            &[],
        )
        .unwrap_or_default();
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        if let Some(path) = r[0].as_string() {
            referenced.insert(path);
        }
    }

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
