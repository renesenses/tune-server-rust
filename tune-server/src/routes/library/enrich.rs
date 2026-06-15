use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::state::AppState;

pub(super) async fn enrich_all_library(State(state): State<AppState>) -> impl IntoResponse {
    let task_id = uuid::Uuid::new_v4().to_string();
    let db = state.db.clone();

    let http_client = state.http_client.clone();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "enrich_all_status",
            &json!({"status": "running", "task_id": task_id, "enriched": 0}).to_string(),
        )
        .ok();

    let db2 = db.clone();
    let task_id_clone = task_id.clone();
    tokio::spawn(async move {
        let track_ids: Vec<(i64, Option<String>)> = {
            let conn = db2.connection().lock().unwrap();
            conn.prepare("SELECT id, file_path FROM tracks WHERE (musicbrainz_recording_id IS NULL OR musicbrainz_recording_id = '') AND file_path IS NOT NULL")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                })
                .unwrap_or_default()
        };

        let total = track_ids.len();

        let mut enriched = 0i32;
        for (track_id, file_path) in &track_ids {
            if let Some(fp) = file_path {
                let meta = tune_core::metadata::read_metadata(std::path::Path::new(fp));
                if let Some(m) = meta {
                    if let Some(ref mbid) = m.musicbrainz_recording_id {
                        db2.execute(
                            "UPDATE tracks SET musicbrainz_recording_id = ? WHERE id = ?",
                            &[mbid as &dyn rusqlite::types::ToSql, track_id],
                        )
                        .ok();
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
                            if let Ok(r) = http_client.get(&url).send().await {
                                if r.status().is_success() {
                                    if let Ok(data) = r.json::<Value>().await {
                                        if let Some(mbid) = data
                                            .pointer("/recordings/0/id")
                                            .and_then(|v| v.as_str())
                                        {
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
                settings
                    .set(
                        "enrich_all_status",
                        &json!({
                            "status": "running",
                            "task_id": task_id_clone,
                            "enriched": enriched,
                            "total": total,
                        })
                        .to_string(),
                    )
                    .ok();
            }
        }

        let settings = tune_core::db::settings_repo::SettingsRepo::new(db2);
        settings
            .set(
                "enrich_all_status",
                &json!({
                    "status": "done",
                    "task_id": task_id_clone,
                    "enriched": enriched,
                    "total": total,
                })
                .to_string(),
            )
            .ok();
        tracing::info!(task_id = %task_id_clone, enriched, total, "enrich_all_library done");
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
