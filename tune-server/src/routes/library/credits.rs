use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::track_repo::TrackRepo;

pub(super) async fn track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let rows = state
        .backend
        .query_many(
            "SELECT id, track_id, artist_id, artist_name, role, instrument, position FROM track_credits WHERE track_id = ? ORDER BY position",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "track_id": r.get(1).and_then(|v| v.as_i64()),
                "artist_id": r.get(2).and_then(|v| v.as_i64()),
                "artist_name": r.get(3).and_then(|v| v.as_string()),
                "role": r.get(4).and_then(|v| v.as_string()),
                "instrument": r.get(5).and_then(|v| v.as_string()),
                "position": r.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

pub(super) async fn artist_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let rows = state
        .backend
        .query_many(
            "SELECT tc.id, tc.track_id, tc.artist_id, tc.artist_name, tc.role, tc.instrument, tc.position \
             FROM track_credits tc \
             WHERE tc.artist_id = ? OR tc.artist_name = (SELECT name FROM artists WHERE id = ?) \
             ORDER BY tc.track_id, tc.position",
            &[&id as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "track_id": r.get(1).and_then(|v| v.as_i64()),
                "artist_id": r.get(2).and_then(|v| v.as_i64()),
                "artist_name": r.get(3).and_then(|v| v.as_string()),
                "role": r.get(4).and_then(|v| v.as_string()),
                "instrument": r.get(5).and_then(|v| v.as_string()),
                "position": r.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

pub(super) async fn enrich_track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return Json(json!({"enriched": false, "reason": "track not found"})).into_response(),
    };

    let Some(ref mbid) = track.musicbrainz_recording_id else {
        return Json(json!({"enriched": false, "reason": "no MusicBrainz recording ID"}))
            .into_response();
    };

    let url = format!(
        "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
    );

    let resp =
        match state.http_client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(data) => data,
                Err(_) => {
                    return Json(
                        json!({"enriched": false, "reason": "invalid MusicBrainz response"}),
                    )
                    .into_response();
                }
            },
            Ok(r) => return Json(
                json!({"enriched": false, "reason": format!("MusicBrainz HTTP {}", r.status())}),
            )
            .into_response(),
            Err(e) => return Json(
                json!({"enriched": false, "reason": format!("MusicBrainz request failed: {e}")}),
            )
            .into_response(),
        };

    // Clear existing credits for this track
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "DELETE FROM track_credits WHERE track_id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok();

    let mut count = 0i32;

    // Parse artist-credits
    if let Some(credits) = resp.get("artist-credit").and_then(|v| v.as_array()) {
        for (pos, credit) in credits.iter().enumerate() {
            let artist_name = credit
                .get("name")
                .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown");
            state.backend.execute(
                "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                &[&id as &dyn ToSqlValue, &artist_name as &dyn ToSqlValue, &(pos as i32) as &dyn ToSqlValue],
            ).ok();
            count += 1;
        }
    }

    // Parse relations for performer/instrument roles
    if let Some(relations) = resp.get("relations").and_then(|v| v.as_array()) {
        for rel in relations {
            let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let artist_name = rel
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(|v| v.as_str());
            if let Some(name) = artist_name {
                let instrument = rel
                    .get("attributes")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                state.backend.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, ?)",
                    &[
                        &id as &dyn ToSqlValue,
                        &name as &dyn ToSqlValue,
                        &rel_type as &dyn ToSqlValue,
                        &instrument as &dyn ToSqlValue,
                        &count as &dyn ToSqlValue,
                    ],
                ).ok();
                count += 1;
            }
        }
    }

    Json(json!({"enriched": true, "credits_count": count})).into_response()
}

pub(super) async fn enrich_album_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();

    let mut enriched = 0i32;
    let mut skipped = 0i32;
    let mut failed = 0i32;
    let total = tracks.len() as i32;

    for track in &tracks {
        let track_id = match track.id {
            Some(id) => id,
            None => {
                skipped += 1;
                continue;
            }
        };

        let Some(ref mbid) = track.musicbrainz_recording_id else {
            skipped += 1;
            continue;
        };

        let url = format!(
            "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
        );

        let resp = match state.http_client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<Value>().await {
                Ok(data) => data,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            },
            _ => {
                failed += 1;
                continue;
            }
        };

        state
            .backend
            .execute(
                "DELETE FROM track_credits WHERE track_id = ?",
                &[&track_id as &dyn ToSqlValue],
            )
            .ok();

        if let Some(credits) = resp.get("artist-credit").and_then(|v| v.as_array()) {
            for (pos, credit) in credits.iter().enumerate() {
                let artist_name = credit
                    .get("name")
                    .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                state.backend.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                    &[&track_id as &dyn ToSqlValue, &artist_name as &dyn ToSqlValue, &(pos as i32) as &dyn ToSqlValue],
                ).ok();
            }
        }

        if let Some(relations) = resp.get("relations").and_then(|v| v.as_array()) {
            for rel in relations {
                let rel_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let artist_name = rel
                    .get("artist")
                    .and_then(|a| a.get("name"))
                    .and_then(|v| v.as_str());
                if let Some(name) = artist_name {
                    let instrument = rel
                        .get("attributes")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    state.backend.execute(
                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                        &[&track_id as &dyn ToSqlValue, &name as &dyn ToSqlValue, &rel_type as &dyn ToSqlValue, &instrument as &dyn ToSqlValue],
                    ).ok();
                }
            }
        }

        enriched += 1;

        // MusicBrainz rate limit: 1 request/sec
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    }

    Json(json!({
        "album_id": id,
        "total": total,
        "enriched": enriched,
        "skipped": skipped,
        "failed": failed,
    }))
}

pub(super) async fn enrich_all_credits(State(state): State<AppState>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let task_id = uuid::Uuid::new_v4().to_string();
    let task_id_clone = task_id.clone();
    let backend = state.backend.clone();

    tokio::spawn(async move {
        let track_ids: Vec<(i64, String)> = backend
            .query_many(
                "SELECT id, musicbrainz_recording_id FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''",
                &[],
            )
            .unwrap_or_default()
            .into_iter()
            .filter_map(|r| {
                let id = r.get(0).and_then(|v| v.as_i64())?;
                let mbid = r.get(1).and_then(|v| v.as_string())?;
                Some((id, mbid))
            })
            .collect();

        let mut enriched = 0i32;
        for (track_id, mbid) in &track_ids {
            let url = format!(
                "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
            );

            if let Ok(r) = state.http_client.get(&url).send().await {
                if r.status().is_success() {
                    if let Ok(data) = r.json::<Value>().await {
                        backend
                            .execute(
                                "DELETE FROM track_credits WHERE track_id = ?",
                                &[track_id as &dyn ToSqlValue],
                            )
                            .ok();

                        if let Some(credits) = data.get("artist-credit").and_then(|v| v.as_array())
                        {
                            for (pos, credit) in credits.iter().enumerate() {
                                let artist_name = credit
                                    .get("name")
                                    .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Unknown");
                                backend.execute(
                                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                                    &[track_id as &dyn ToSqlValue, &artist_name as &dyn ToSqlValue, &(pos as i32) as &dyn ToSqlValue],
                                ).ok();
                            }
                        }

                        if let Some(relations) = data.get("relations").and_then(|v| v.as_array()) {
                            for rel in relations {
                                let rel_type =
                                    rel.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                let artist_name = rel
                                    .get("artist")
                                    .and_then(|a| a.get("name"))
                                    .and_then(|v| v.as_str());
                                if let Some(name) = artist_name {
                                    let instrument = rel
                                        .get("attributes")
                                        .and_then(|v| v.as_array())
                                        .and_then(|arr| arr.first())
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string());
                                    backend.execute(
                                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                                        &[track_id as &dyn ToSqlValue, &name as &dyn ToSqlValue, &rel_type as &dyn ToSqlValue, &instrument as &dyn ToSqlValue],
                                    ).ok();
                                }
                            }
                        }

                        enriched += 1;
                    }
                }
            }

            // MusicBrainz rate limit: 1 request/sec
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }

        tracing::info!(task_id = %task_id_clone, enriched, total = track_ids.len(), "enrich_all_credits_done");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "task_id": task_id})),
    )
}
