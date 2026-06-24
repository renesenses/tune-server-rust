use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::tag_writer::{TagUpdate, write_tags};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub(super) struct WriteTagsRequest {
    /// If true (default), only write fields that are currently empty in
    /// the file's tags. If false, overwrite all fields from DB metadata.
    #[serde(default = "default_true")]
    pub only_missing: bool,
    /// Specific track IDs to process. `None` means all tracks.
    pub track_ids: Option<Vec<i64>>,
}

fn default_true() -> bool {
    true
}

/// POST /library/write-tags
///
/// Writes metadata from the DB back to audio files' tags using lofty.
/// Reads current file tags first, then only fills in missing fields
/// (when `only_missing` is true).
pub(super) async fn write_tags_to_files(
    State(state): State<AppState>,
    Json(body): Json<WriteTagsRequest>,
) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let backend = state.backend.clone();

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "write_tags_status",
            &json!({"status": "running", "task_id": task_id, "written": 0}).to_string(),
        )
        .ok();

    let backend2 = backend.clone();
    let task_id_clone = task_id.clone();
    let only_missing = body.only_missing;
    let track_ids = body.track_ids;

    tokio::spawn(async move {
        // Build the SQL query based on whether specific track IDs were given
        let track_rows = if let Some(ref ids) = track_ids {
            if ids.is_empty() {
                vec![]
            } else {
                // Build IN clause with placeholders
                let placeholders: Vec<String> = ids.iter().map(|_| "?".to_string()).collect();
                let sql = format!(
                    "SELECT id, file_path, title, artist_name, album_title, \
                     track_number, disc_number, genre, composer, year, label, \
                     comment \
                     FROM tracks WHERE file_path IS NOT NULL AND id IN ({})",
                    placeholders.join(",")
                );
                let params: Vec<Box<dyn ToSqlValue>> = ids
                    .iter()
                    .map(|id| Box::new(*id) as Box<dyn ToSqlValue>)
                    .collect();
                let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();
                backend2.query_many(&sql, &param_refs).unwrap_or_default()
            }
        } else {
            backend2
                .query_many(
                    "SELECT id, file_path, title, artist_name, album_title, \
                     track_number, disc_number, genre, composer, year, label, \
                     comment \
                     FROM tracks WHERE file_path IS NOT NULL",
                    &[],
                )
                .unwrap_or_default()
        };

        let total = track_rows.len();
        let mut written = 0i32;
        let mut skipped = 0i32;
        let mut errors = 0i32;

        for row in &track_rows {
            let track_id = row.get(0).and_then(|v| v.as_i64()).unwrap_or(0);
            let file_path = match row.get(1).and_then(|v| v.as_string()) {
                Some(fp) => fp,
                None => continue,
            };
            let title = row.get(2).and_then(|v| v.as_string());
            let artist_name = row.get(3).and_then(|v| v.as_string());
            let album_title = row.get(4).and_then(|v| v.as_string());
            let track_number = row.get(5).and_then(|v| v.as_i64()).map(|v| v as i32);
            let disc_number = row.get(6).and_then(|v| v.as_i64()).map(|v| v as i32);
            let genre = row.get(7).and_then(|v| v.as_string());
            let composer = row.get(8).and_then(|v| v.as_string());
            let year = row.get(9).and_then(|v| v.as_i64()).map(|v| v as i32);
            let _label = row.get(10).and_then(|v| v.as_string());
            let comment = row.get(11).and_then(|v| v.as_string());

            // Check file exists
            if !std::path::Path::new(&file_path).exists() {
                debug!(track_id, file_path = %file_path, "write_tags_file_not_found");
                skipped += 1;
                continue;
            }

            if only_missing {
                // Read current tags from file to check what's missing
                let current_tags = match tune_core::metadata::tag_writer::read_tags(&file_path)
                    .await
                {
                    Ok(tags) => tags,
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_read_failed");
                        errors += 1;
                        continue;
                    }
                };

                // Build update with only fields missing from the file
                let update = TagUpdate {
                    title: if current_tags.get("title").map_or(true, |v| v.is_empty()) {
                        title
                    } else {
                        None
                    },
                    artist_name: if current_tags.get("artist").map_or(true, |v| v.is_empty()) {
                        artist_name
                    } else {
                        None
                    },
                    album_title: if current_tags.get("album").map_or(true, |v| v.is_empty()) {
                        album_title
                    } else {
                        None
                    },
                    track_number: if current_tags
                        .get("tracknumber")
                        .map_or(true, |v| v.is_empty())
                    {
                        track_number
                    } else {
                        None
                    },
                    disc_number: if current_tags
                        .get("discnumber")
                        .map_or(true, |v| v.is_empty())
                    {
                        disc_number
                    } else {
                        None
                    },
                    genre: if current_tags.get("genre").map_or(true, |v| v.is_empty()) {
                        genre
                    } else {
                        None
                    },
                    composer: if current_tags.get("composer").map_or(true, |v| v.is_empty()) {
                        composer
                    } else {
                        None
                    },
                    year: if current_tags.get("date").map_or(true, |v| v.is_empty()) {
                        year
                    } else {
                        None
                    },
                    comment: if current_tags.get("comment").map_or(true, |v| v.is_empty()) {
                        comment
                    } else {
                        None
                    },
                    label: None, // label/isrc/bpm/lyrics handled by extended writer
                    isrc: None,
                    bpm: None,
                    lyrics: None,
                };

                // Skip if nothing to write
                if update.title.is_none()
                    && update.artist_name.is_none()
                    && update.album_title.is_none()
                    && update.track_number.is_none()
                    && update.disc_number.is_none()
                    && update.genre.is_none()
                    && update.composer.is_none()
                    && update.year.is_none()
                    && update.comment.is_none()
                {
                    skipped += 1;
                    continue;
                }

                match write_tags(&file_path, &update).await {
                    Ok(result) => {
                        written += 1;
                        debug!(
                            track_id,
                            file_path = %file_path,
                            fields = result.fields_written,
                            "tags_written"
                        );
                    }
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_failed");
                        errors += 1;
                    }
                }
            } else {
                // Overwrite mode: write all DB fields to file
                let update = TagUpdate {
                    title,
                    artist_name,
                    album_title,
                    track_number,
                    disc_number,
                    genre,
                    composer,
                    year,
                    comment,
                    label: None,
                    isrc: None,
                    bpm: None,
                    lyrics: None,
                };

                match write_tags(&file_path, &update).await {
                    Ok(result) => {
                        written += 1;
                        debug!(
                            track_id,
                            file_path = %file_path,
                            fields = result.fields_written,
                            "tags_written_overwrite"
                        );
                    }
                    Err(e) => {
                        warn!(track_id, file_path = %file_path, error = %e, "write_tags_failed");
                        errors += 1;
                    }
                }
            }

            // Update status periodically
            if (written + errors) % 50 == 0 {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend2.clone());
                settings
                    .set(
                        "write_tags_status",
                        &json!({
                            "status": "running",
                            "task_id": task_id_clone,
                            "written": written,
                            "skipped": skipped,
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
                "write_tags_status",
                &json!({
                    "status": "done",
                    "task_id": task_id_clone,
                    "written": written,
                    "skipped": skipped,
                    "errors": errors,
                    "total": total,
                })
                .to_string(),
            )
            .ok();
        info!(
            task_id = %task_id_clone,
            written,
            skipped,
            errors,
            total,
            "write_tags_to_files done"
        );
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "task_id": task_id})),
    )
}

/// GET /library/write-tags/status
pub(super) async fn write_tags_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("write_tags_status")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({"status": "idle"}));
    Json(result)
}
