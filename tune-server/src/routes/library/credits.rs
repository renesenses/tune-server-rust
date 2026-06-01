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
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare("SELECT id, track_id, artist_id, artist_name, role, instrument, position FROM track_credits WHERE track_id = ? ORDER BY position")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![id], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "track_id": row.get::<_, Option<i64>>(1).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(3).ok().flatten(),
                    "role": row.get::<_, Option<String>>(4).ok().flatten(),
                    "instrument": row.get::<_, Option<String>>(5).ok().flatten(),
                    "position": row.get::<_, Option<i32>>(6).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

pub(super) async fn artist_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare(
            "SELECT tc.id, tc.track_id, tc.artist_id, tc.artist_name, tc.role, tc.instrument, tc.position \
             FROM track_credits tc \
             WHERE tc.artist_id = ? OR tc.artist_name = (SELECT name FROM artists WHERE id = ?) \
             ORDER BY tc.track_id, tc.position"
        )
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![id, id], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "track_id": row.get::<_, Option<i64>>(1).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(3).ok().flatten(),
                    "role": row.get::<_, Option<String>>(4).ok().flatten(),
                    "instrument": row.get::<_, Option<String>>(5).ok().flatten(),
                    "position": row.get::<_, Option<i32>>(6).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

pub(super) async fn enrich_track_credits(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());
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
    state
        .db
        .execute(
            "DELETE FROM track_credits WHERE track_id = ?",
            &[&id as &dyn rusqlite::types::ToSql],
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
            state.db.execute(
                "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                &[&id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
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
                state.db.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, ?)",
                    &[
                        &id as &dyn rusqlite::types::ToSql,
                        &name,
                        &rel_type,
                        &instrument,
                        &count,
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
    let track_repo = TrackRepo::new(state.db.clone());
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
            .db
            .execute(
                "DELETE FROM track_credits WHERE track_id = ?",
                &[&track_id as &dyn rusqlite::types::ToSql],
            )
            .ok();

        if let Some(credits) = resp.get("artist-credit").and_then(|v| v.as_array()) {
            for (pos, credit) in credits.iter().enumerate() {
                let artist_name = credit
                    .get("name")
                    .or_else(|| credit.get("artist").and_then(|a| a.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown");
                state.db.execute(
                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                    &[&track_id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
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
                    state.db.execute(
                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                        &[&track_id as &dyn rusqlite::types::ToSql, &name, &rel_type, &instrument],
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
    let task_id = uuid::Uuid::new_v4().to_string();
    let task_id_clone = task_id.clone();
    let db = state.db.clone();

    tokio::spawn(async move {
        let track_ids: Vec<(i64, String)> = {
            let conn = db.connection().lock().unwrap();
            conn
                .prepare("SELECT id, musicbrainz_recording_id FROM tracks WHERE musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id != ''")
                .and_then(|mut stmt| {
                    stmt.query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                })
                .unwrap_or_default()
        };

        let mut enriched = 0i32;
        for (track_id, mbid) in &track_ids {
            let url = format!(
                "https://musicbrainz.org/ws/2/recording/{mbid}?inc=artist-credits+artist-rels&fmt=json"
            );

            if let Ok(r) = state.http_client.get(&url).send().await {
                if r.status().is_success() {
                    if let Ok(data) = r.json::<Value>().await {
                        db.execute(
                            "DELETE FROM track_credits WHERE track_id = ?",
                            &[track_id as &dyn rusqlite::types::ToSql],
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
                                db.execute(
                                    "INSERT INTO track_credits (track_id, artist_name, role, position) VALUES (?, ?, 'artist', ?)",
                                    &[track_id as &dyn rusqlite::types::ToSql, &artist_name, &(pos as i32)],
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
                                    db.execute(
                                        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?, ?, ?, ?, 0)",
                                        &[track_id as &dyn rusqlite::types::ToSql, &name, &rel_type, &instrument],
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
