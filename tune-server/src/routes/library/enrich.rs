use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{debug, info, warn};

use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::enrichment::RecordingDetails;

use crate::state::AppState;

const MUSICBRAINZ_API: &str = "https://musicbrainz.org/ws/2";
const MB_USER_AGENT: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";
const MB_RATE_LIMIT_MS: u64 = 1100;

/// POST /library/enrich-all
///
/// Enriches tracks with metadata from MusicBrainz. Finds tracks with
/// missing metadata (MB ID, genre, year, label) and fetches details.
/// For tracks that already have a MB recording ID, fetches details
/// directly. For tracks without, does a lookup first.
///
/// Updates DB with ALL enriched fields using COALESCE to never
/// overwrite existing data.
pub(super) async fn enrich_all_library(State(state): State<AppState>) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let backend = state.backend.clone();
    let http_client = state.http_client.clone();

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "enrich_all_status",
            &json!({"status": "running", "task_id": task_id, "enriched": 0}).to_string(),
        )
        .ok();

    let backend2 = backend.clone();
    let task_id_clone = task_id.clone();
    tokio::spawn(async move {
        // Find tracks with missing metadata: no MB ID OR missing genre/year/label
        let track_rows: Vec<Vec<tune_core::db::backend::SqlValue>> = backend2
            .query_many(
                "SELECT t.id, t.title, t.artist_name, t.album_title, t.file_path, \
                 t.musicbrainz_recording_id, t.genre, t.year, t.label, t.composer, t.album_id, \
                 t.artist_id, a.musicbrainz_id \
                 FROM tracks t \
                 LEFT JOIN artists a ON a.id = t.artist_id \
                 WHERE t.file_path IS NOT NULL AND ( \
                   t.musicbrainz_recording_id IS NULL OR t.musicbrainz_recording_id = '' \
                   OR t.genre IS NULL OR t.genre = '' \
                   OR t.year IS NULL \
                   OR t.label IS NULL OR t.label = '' \
                   OR (t.artist_id IS NOT NULL AND (a.musicbrainz_id IS NULL OR a.musicbrainz_id = '')) \
                 )",
                &[],
            )
            .unwrap_or_default();

        let total = track_rows.len();

        // Build a dedicated HTTP client with proper UA for MusicBrainz
        let mb_client = tune_core::http::client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(MB_USER_AGENT)
            .build()
            .unwrap_or_else(|_| http_client.clone());

        let mut enriched = 0i32;
        let mut errors = 0i32;
        // Artists whose MBID we've already backfilled this run, so we don't
        // re-fetch recording details once per track for the same artist.
        let mut artists_mbid_done: std::collections::HashSet<i64> =
            std::collections::HashSet::new();

        for row in &track_rows {
            let track_id = row.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            let title = row.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let artist = row.get(2).and_then(|v| v.as_string());
            let album = row.get(3).and_then(|v| v.as_string());
            let _file_path = row.get(4).and_then(|v| v.as_string());
            let existing_mb_id = row
                .get(5)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let existing_genre = row
                .get(6)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let existing_year = row.get(7).and_then(|v| v.as_i64());
            let existing_label = row
                .get(8)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let _existing_composer = row
                .get(9)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            let artist_id = row.get(11).and_then(|v| v.as_i64());
            let existing_artist_mbid = row
                .get(12)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty());
            // The artist row lacks a MusicBrainz ID and we haven't filled it yet
            // this run — fetching it unlocks Wikipedia/Wikidata artist bios.
            let artist_needs_mbid = match artist_id {
                Some(aid) => existing_artist_mbid.is_none() && !artists_mbid_done.contains(&aid),
                None => false,
            };

            // If track already has all fields (and the artist MBID too), skip
            if existing_mb_id.is_some()
                && existing_genre.is_some()
                && existing_year.is_some()
                && existing_label.is_some()
                && !artist_needs_mbid
            {
                continue;
            }

            let mb_id = if let Some(ref id) = existing_mb_id {
                // Already have MB ID, just need to fetch details
                id.clone()
            } else {
                // Need to look up MB ID first
                if title.is_empty() {
                    continue;
                }
                match mb_lookup_recording(&mb_client, &title, artist.as_deref(), album.as_deref())
                    .await
                {
                    Ok(Some(id)) => {
                        tokio::time::sleep(Duration::from_millis(MB_RATE_LIMIT_MS)).await;
                        id
                    }
                    Ok(None) => {
                        tokio::time::sleep(Duration::from_millis(MB_RATE_LIMIT_MS)).await;
                        continue;
                    }
                    Err(e) => {
                        warn!(track_id, error = %e, "mb_lookup_failed");
                        errors += 1;
                        tokio::time::sleep(Duration::from_millis(MB_RATE_LIMIT_MS)).await;
                        continue;
                    }
                }
            };

            // Fetch recording details if we're missing genre/year/label
            let needs_details = existing_genre.is_none()
                || existing_year.is_none()
                || existing_label.is_none()
                || artist_needs_mbid;

            let details = if needs_details {
                match mb_fetch_recording_details(&mb_client, &mb_id).await {
                    Ok(d) => {
                        tokio::time::sleep(Duration::from_millis(MB_RATE_LIMIT_MS)).await;
                        d
                    }
                    Err(e) => {
                        warn!(track_id, mb_id = %mb_id, error = %e, "mb_details_failed");
                        errors += 1;
                        tokio::time::sleep(Duration::from_millis(MB_RATE_LIMIT_MS)).await;
                        RecordingDetails::default()
                    }
                }
            } else {
                RecordingDetails::default()
            };

            // Update tracks DB with COALESCE so we never overwrite existing data
            let genre_val: Option<String> = details.genre.clone();
            let year_val: Option<i32> = details.year;
            let label_val: Option<String> = details.label.clone();
            let composer_val: Option<String> = details.composer.clone();
            let mb_id_val: Option<String> = Some(mb_id.clone());
            let isrc_val: Option<String> = details.isrc.clone();

            let result = backend2.execute(
                "UPDATE tracks SET \
                 genre = COALESCE(genre, ?), \
                 year = COALESCE(year, ?), \
                 label = COALESCE(label, ?), \
                 composer = COALESCE(composer, ?), \
                 isrc = COALESCE(isrc, ?), \
                 musicbrainz_recording_id = COALESCE(musicbrainz_recording_id, ?) \
                 WHERE id = ?",
                &[
                    &genre_val as &dyn ToSqlValue,
                    &year_val as &dyn ToSqlValue,
                    &label_val as &dyn ToSqlValue,
                    &composer_val as &dyn ToSqlValue,
                    &isrc_val as &dyn ToSqlValue,
                    &mb_id_val as &dyn ToSqlValue,
                    &track_id as &dyn ToSqlValue,
                ],
            );

            // Also update album with release-level metadata
            if let Some(album_id) = row.get(10).and_then(|v| v.as_i64()) {
                let release_id = details.release_id.clone();
                let release_group_id = details.release_group_id.clone();
                let catalog_number = details.catalog_number.clone();
                let barcode = details.barcode.clone();
                let album_label = details.label.clone();
                let original_year = details.original_year;
                backend2
                    .execute(
                        "UPDATE albums SET \
                     musicbrainz_release_id = COALESCE(musicbrainz_release_id, ?), \
                     musicbrainz_release_group_id = COALESCE(musicbrainz_release_group_id, ?), \
                     catalog_number = COALESCE(catalog_number, ?), \
                     barcode = COALESCE(barcode, ?), \
                     label = COALESCE(label, ?), \
                     original_year = COALESCE(original_year, ?) \
                     WHERE id = ?",
                        &[
                            &release_id as &dyn ToSqlValue,
                            &release_group_id as &dyn ToSqlValue,
                            &catalog_number as &dyn ToSqlValue,
                            &barcode as &dyn ToSqlValue,
                            &album_label as &dyn ToSqlValue,
                            &original_year as &dyn ToSqlValue,
                            &album_id as &dyn ToSqlValue,
                        ],
                    )
                    .ok();
            }

            // Backfill the artist's MusicBrainz ID (unlocks Wikipedia/Wikidata
            // bios). COALESCE so an existing value is never overwritten.
            if artist_needs_mbid {
                if let (Some(aid), Some(artist_mbid)) =
                    (artist_id, details.musicbrainz_artist_id.as_ref())
                {
                    let ambid_val: Option<String> = Some(artist_mbid.clone());
                    backend2
                        .execute(
                            "UPDATE artists SET musicbrainz_id = COALESCE(musicbrainz_id, ?) \
                             WHERE id = ?",
                            &[&ambid_val as &dyn ToSqlValue, &aid as &dyn ToSqlValue],
                        )
                        .ok();
                    artists_mbid_done.insert(aid);
                }
            }

            match result {
                Ok(_) => {
                    enriched += 1;
                    debug!(
                        track_id,
                        mb_id = %mb_id,
                        genre = ?details.genre,
                        year = ?details.year,
                        label = ?details.label,
                        "track_enriched"
                    );
                }
                Err(e) => {
                    warn!(track_id, error = %e, "enrich_db_update_failed");
                    errors += 1;
                }
            }

            // Update status periodically
            if enriched % 50 == 0 {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend2.clone());
                settings
                    .set(
                        "enrich_all_status",
                        &json!({
                            "status": "running",
                            "task_id": task_id_clone,
                            "enriched": enriched,
                            "errors": errors,
                            "total": total,
                        })
                        .to_string(),
                    )
                    .ok();
            }
        }

        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend2);
        settings
            .set(
                "enrich_all_status",
                &json!({
                    "status": "done",
                    "task_id": task_id_clone,
                    "enriched": enriched,
                    "errors": errors,
                    "total": total,
                })
                .to_string(),
            )
            .ok();
        info!(task_id = %task_id_clone, enriched, errors, total, "enrich_all_library done");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "task_id": task_id})),
    )
}

pub(super) async fn enrich_all_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("enrich_all_status")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}));
    Json(result)
}

// ── MusicBrainz helper functions (standalone, no MetadataEnricher needed) ──

/// Look up a recording on MusicBrainz by title + artist + album.
/// Returns the recording ID if found.
async fn mb_lookup_recording(
    client: &reqwest::Client,
    title: &str,
    artist: Option<&str>,
    album: Option<&str>,
) -> Result<Option<String>, String> {
    let mut query_parts = vec![format!("recording:{title}")];
    if let Some(a) = artist {
        if !a.is_empty() {
            query_parts.push(format!("artist:{a}"));
        }
    }
    if let Some(al) = album {
        if !al.is_empty() {
            query_parts.push(format!("release:{al}"));
        }
    }
    let query = query_parts.join(" AND ");

    let resp = client
        .get(format!("{MUSICBRAINZ_API}/recording"))
        .query(&[
            ("query", &query),
            ("fmt", &"json".to_string()),
            ("limit", &"1".to_string()),
        ])
        .send()
        .await
        .map_err(|e| format!("mb lookup: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let data: Value = resp.json().await.map_err(|e| format!("mb parse: {e}"))?;
    let recording_id = data["recordings"]
        .as_array()
        .and_then(|recs| recs.first())
        .and_then(|r| r["id"].as_str())
        .map(String::from);

    Ok(recording_id)
}

/// Fetch detailed metadata for a MusicBrainz recording.
async fn mb_fetch_recording_details(
    client: &reqwest::Client,
    recording_id: &str,
) -> Result<RecordingDetails, String> {
    let url = format!("{MUSICBRAINZ_API}/recording/{recording_id}");
    let resp = client
        .get(&url)
        .query(&[("inc", "releases+tags+artist-credits"), ("fmt", "json")])
        .send()
        .await
        .map_err(|e| format!("mb details: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("mb details: HTTP {}", resp.status()));
    }

    let data: Value = resp.json().await.map_err(|e| format!("mb parse: {e}"))?;

    // Genre: highest-count tag
    let genre = pick_best_genre(&data["tags"]);

    // First release for year/label/IDs
    let first_release = data["releases"].as_array().and_then(|arr| arr.first());

    let year = first_release
        .and_then(|r| r["date"].as_str())
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<i32>().ok());

    let label = first_release
        .and_then(|r| r["label-info"].as_array())
        .and_then(|arr| arr.first())
        .and_then(|li| li["label"]["name"].as_str())
        .map(String::from);

    let release_id = first_release
        .and_then(|r| r["id"].as_str())
        .map(String::from);

    let release_group_id = first_release
        .and_then(|r| r["release-group"]["id"].as_str())
        .map(String::from);

    let isrc = data["isrcs"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(String::from);

    let catalog_number = first_release
        .and_then(|r| r["label-info"].as_array())
        .and_then(|arr| arr.first())
        .and_then(|li| li["catalog-number"].as_str())
        .map(String::from);

    let barcode = first_release
        .and_then(|r| r["barcode"].as_str())
        .filter(|b| !b.is_empty())
        .map(String::from);

    let country = first_release
        .and_then(|r| r["country"].as_str())
        .map(String::from);

    let original_year = first_release
        .and_then(|r| r["release-group"]["first-release-date"].as_str())
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<i32>().ok());

    let musicbrainz_artist_id = data["artist-credit"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|ac| ac["artist"]["id"].as_str())
        .map(String::from);

    Ok(RecordingDetails {
        genre,
        year,
        original_year,
        label,
        catalog_number,
        barcode,
        country,
        composer: None,
        isrc,
        release_id,
        release_group_id,
        musicbrainz_artist_id,
    })
}

/// Pick the best genre from a MusicBrainz `tags` array.
fn pick_best_genre(tags_value: &Value) -> Option<String> {
    let tags = tags_value.as_array()?;
    tags.iter()
        .filter_map(|t| {
            let name = t["name"].as_str()?;
            let count = t["count"].as_i64().unwrap_or(0);
            if name.len() < 2 {
                return None;
            }
            Some((name.to_string(), count))
        })
        .max_by_key(|(_, count)| *count)
        .map(|(name, _)| tune_core::metadata::normalize_genre(&name))
}
