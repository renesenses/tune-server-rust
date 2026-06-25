use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::license::Feature;

use crate::error::AppError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Free-tier daily enrichment limit
// ---------------------------------------------------------------------------

const FREE_DAILY_ENRICHMENT_LIMIT: i64 = 10;
const ENRICHMENT_COUNT_KEY: &str = "enrichment_daily_count";
const ENRICHMENT_DATE_KEY: &str = "enrichment_daily_date";

/// Returns (count_used_today, limit). Resets counter if the date has changed.
fn get_daily_enrichment_usage(settings: &SettingsRepo) -> (i64, i64) {
    let today = today_utc_str();
    let stored_date = settings
        .get(ENRICHMENT_DATE_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();

    if stored_date != today {
        // New day — reset counter
        settings.set(ENRICHMENT_DATE_KEY, &today).ok();
        settings.set(ENRICHMENT_COUNT_KEY, "0").ok();
        return (0, FREE_DAILY_ENRICHMENT_LIMIT);
    }

    let count: i64 = settings
        .get(ENRICHMENT_COUNT_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    (count, FREE_DAILY_ENRICHMENT_LIMIT)
}

/// Increment the daily enrichment counter by `n`.
fn increment_daily_enrichment(settings: &SettingsRepo, n: i64) {
    let (current, _) = get_daily_enrichment_usage(settings);
    let new_count = current + n;
    settings
        .set(ENRICHMENT_COUNT_KEY, &new_count.to_string())
        .ok();
}

// ---------------------------------------------------------------------------
// POST /system/enrich — artwork enrichment (MBID + covers)
// ---------------------------------------------------------------------------

pub(super) async fn system_enrich(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    if !is_premium {
        let (used, limit) = get_daily_enrichment_usage(&settings);
        if used >= limit {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": "free_tier_daily_enrichment_limit_reached",
                    "used": used,
                    "limit": limit,
                    "upgrade": "Premium unlocks unlimited auto enrichment",
                })),
            );
        }
        // Free tier: increment and proceed (limited scope)
        increment_daily_enrichment(&settings, 1);
    }

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
        Json(json!({
            "status": "enrichment_started",
            "premium": is_premium,
        })),
    )
}

// ---------------------------------------------------------------------------
// POST /system/enrich-bios — bio enrichment
// ---------------------------------------------------------------------------

pub(super) async fn enrich_bios(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    if !is_premium {
        let (used, limit) = get_daily_enrichment_usage(&settings);
        if used >= limit {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": "free_tier_daily_enrichment_limit_reached",
                    "used": used,
                    "limit": limit,
                    "upgrade": "Premium unlocks unlimited auto enrichment",
                })),
            );
        }
        increment_daily_enrichment(&settings, 1);
    }

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
            "premium": is_premium,
        })),
    )
}

// ---------------------------------------------------------------------------
// POST /system/enrich-metadata — extended file metadata extraction
// ---------------------------------------------------------------------------

pub(super) async fn enrich_extended_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    if !is_premium {
        let (used, limit) = get_daily_enrichment_usage(&settings);
        if used >= limit {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": "free_tier_daily_enrichment_limit_reached",
                    "used": used,
                    "limit": limit,
                    "upgrade": "Premium unlocks unlimited auto enrichment",
                })),
            );
        }
        increment_daily_enrichment(&settings, 1);
    }

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
        let mut batch: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();
        for (track_id, file_path) in &tracks {
            let path = std::path::Path::new(file_path);
            if !path.exists() {
                continue;
            }
            let ext =
                tokio::task::block_in_place(|| tune_core::metadata::read_extended_metadata(path));
            if !ext.is_empty() {
                batch.push((*track_id, ext));
                enriched += 1;
            }
            if batch.len() >= 500 {
                if let Err(e) = meta_repo.set_batch_multi(&batch) {
                    tracing::error!(error = %e, "enrich_metadata_batch_failed");
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            if let Err(e) = meta_repo.set_batch_multi(&batch) {
                tracing::error!(error = %e, "enrich_metadata_batch_failed");
            }
        }
        tracing::info!(total, enriched, "enrich_extended_metadata_complete");
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "extended_metadata_enrichment_started",
            "premium": is_premium,
        })),
    )
}

// ---------------------------------------------------------------------------
// GET /system/enrichment/status — enrichment statistics
// ---------------------------------------------------------------------------

pub(super) async fn enrichment_status(State(state): State<AppState>) -> Json<Value> {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let (daily_used, daily_limit) = get_daily_enrichment_usage(&settings);

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let total_tracks = track_repo.count().unwrap_or(0);
    let total_artists = artist_repo.count().unwrap_or(0);
    let total_albums = album_repo.count().unwrap_or(0);

    // Artists with bios
    let artists_with_bio = artist_repo
        .list_without_bio()
        .map(|v| total_artists - v.len() as i64)
        .unwrap_or(0);
    // Artists with images
    let artists_with_image: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM artists WHERE image_path IS NOT NULL AND image_path != ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0);
    // Albums with covers
    let albums_with_cover: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM albums WHERE cover_path IS NOT NULL AND cover_path != ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0);
    // Albums with bios
    let albums_with_bio = album_repo
        .list_without_bio()
        .map(|v| total_albums - v.len() as i64)
        .unwrap_or(0);
    // Artists with MusicBrainz IDs
    let artists_with_mbid: i64 = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id != ''",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0);

    // Last enrichment run timestamp
    let last_run = settings.get("enrichment_last_run").ok().flatten();

    Json(json!({
        "premium": is_premium,
        "daily_used": daily_used,
        "daily_limit": if is_premium { null_i64() } else { Some(daily_limit) },
        "stats": {
            "total_tracks": total_tracks,
            "total_artists": total_artists,
            "total_albums": total_albums,
            "artists_with_bio": artists_with_bio,
            "artists_with_image": artists_with_image,
            "artists_with_mbid": artists_with_mbid,
            "albums_with_cover": albums_with_cover,
            "albums_with_bio": albums_with_bio,
        },
        "last_run": last_run,
    }))
}

/// Helper to produce a JSON null for the daily_limit field on Premium.
fn null_i64() -> Option<i64> {
    None
}

/// Return today's date as "YYYY-MM-DD" in UTC, without chrono dependency.
fn today_utc_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 86400 seconds per day; compute days since epoch and derive date components
    let days = secs / 86400;
    // Civil date from days since 1970-01-01 (Algorithm from Howard Hinnant)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current UTC timestamp as ISO 8601, without chrono dependency.
fn now_utc_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let date = today_utc_str();
    let day_secs = secs % 86400;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{date}T{h:02}:{m:02}:{s:02}Z")
}

// ---------------------------------------------------------------------------
// POST /system/enrichment/run — trigger full enrichment run
// ---------------------------------------------------------------------------

pub(super) async fn enrichment_run(State(state): State<AppState>) -> impl IntoResponse {
    let is_premium = state.license.check_feature(Feature::AutoEnrichment).await;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    if !is_premium {
        let (used, limit) = get_daily_enrichment_usage(&settings);
        if used >= limit {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": "free_tier_daily_enrichment_limit_reached",
                    "used": used,
                    "limit": limit,
                    "upgrade": "Premium unlocks unlimited auto enrichment",
                })),
            );
        }
        increment_daily_enrichment(&settings, 1);
    }

    // Record the run timestamp
    let now = now_utc_str();
    settings.set("enrichment_last_run", &now).ok();

    // 1. Artwork enrichment
    let db1 = state.backend.clone();
    let cache_dir = crate::routes::library::artwork_cache_dir();
    let cache_dir2 = cache_dir.clone();
    tokio::spawn(async move {
        tune_core::library::artwork::batch_enrich_artwork(db1, cache_dir).await;
    });

    // 2. Artist MBID matching + artist artwork
    let mbid_db = state.backend.clone();
    let art_db = state.backend.clone();
    let art_cache = cache_dir2.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::metadata::matcher::batch_match_artist_mbids(mbid_db).await;
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tune_core::library::artwork::batch_enrich_artist_artwork(art_db, art_cache).await;
    });

    // 3. Bio enrichment
    let bio_artist_db = state.backend.clone();
    let bio_album_db = state.backend.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        tune_core::metadata::bio_batch::batch_enrich_artist_bios(bio_artist_db).await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        tune_core::metadata::bio_batch::batch_enrich_album_bios(bio_album_db).await;
    });

    // 4. Extended file metadata
    let ext_db = state.backend.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        let meta_repo =
            tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(ext_db.clone());
        let tracks: Vec<(i64, String)> = ext_db
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
        tracing::info!(total, "enrichment_run_extended_metadata_started");
        let mut enriched = 0u64;
        let mut batch: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();
        for (track_id, file_path) in &tracks {
            let path = std::path::Path::new(file_path);
            if !path.exists() {
                continue;
            }
            let ext =
                tokio::task::block_in_place(|| tune_core::metadata::read_extended_metadata(path));
            if !ext.is_empty() {
                batch.push((*track_id, ext));
                enriched += 1;
            }
            if batch.len() >= 500 {
                if let Err(e) = meta_repo.set_batch_multi(&batch) {
                    tracing::error!(error = %e, "enrichment_run_metadata_batch_failed");
                }
                batch.clear();
            }
        }
        if !batch.is_empty() {
            if let Err(e) = meta_repo.set_batch_multi(&batch) {
                tracing::error!(error = %e, "enrichment_run_metadata_batch_failed");
            }
        }
        tracing::info!(total, enriched, "enrichment_run_extended_metadata_complete");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "enrichment_run_started",
            "premium": is_premium,
            "scope": if is_premium { "full_library" } else { "limited" },
        })),
    )
}

// ---------------------------------------------------------------------------
// POST /system/cleanup — existing cleanup (unchanged)
// ---------------------------------------------------------------------------

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
