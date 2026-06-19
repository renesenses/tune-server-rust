use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateSmartPlaylist {
    name: String,
    rules: Value,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateSmartPlaylist {
    name: Option<String>,
    rules: Option<Value>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_smart_playlists).post(create_smart_playlist))
        .route(
            "/{id}",
            get(get_smart_playlist)
                .put(update_smart_playlist)
                .delete(delete_smart_playlist),
        )
        .route("/{id}/tracks", get(resolve_tracks))
        .route("/{id}/albums", get(smart_collection_albums))
        .route("/preview", post(preview_smart_collection))
}

async fn list_smart_playlists(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare("SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at FROM smart_playlists ORDER BY name")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                let rules_str: String = row.get(2).unwrap_or_else(|_| "[]".into());
                let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "name": row.get::<_, Option<String>>(1).ok().flatten(),
                    "rules": rules,
                    "sort_by": row.get::<_, Option<String>>(3).ok().flatten(),
                    "sort_order": row.get::<_, Option<String>>(4).ok().flatten(),
                    "max_tracks": row.get::<_, Option<i64>>(5).ok().flatten(),
                    "created_at": row.get::<_, Option<String>>(6).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

async fn create_smart_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    let rules_json = body.rules.to_string();
    let sort_by = body.sort_by.clone().unwrap_or_else(|| "title".into());
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    let result = {
        let conn = state
            .db
            .connection()
            .lock()
            .map_err(|e| AppError::internal(format!("{e}")))?;
        conn.execute(
            "INSERT INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![body.name, rules_json, sort_by, sort_order, body.max_tracks],
        )
        .map(|_| conn.last_insert_rowid())
        .map_err(|e| e.to_string())
    };

    match result {
        Ok(id) => {
            let created = json!({
                "id": id,
                "name": body.name,
                "rules": body.rules,
                "sort_by": sort_by,
                "sort_order": sort_order,
                "max_tracks": body.max_tracks,
            });
            Ok((StatusCode::CREATED, Json(created)).into_response())
        }
        Err(e) => Err(AppError::internal(e)),
    }
}

async fn get_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let result = conn.query_row(
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at FROM smart_playlists WHERE id = ?",
        rusqlite::params![id],
        |row| {
            let rules_str: String = row.get(2).unwrap_or_else(|_| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(json!({
                "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                "name": row.get::<_, Option<String>>(1).ok().flatten(),
                "rules": rules,
                "sort_by": row.get::<_, Option<String>>(3).ok().flatten(),
                "sort_order": row.get::<_, Option<String>>(4).ok().flatten(),
                "max_tracks": row.get::<_, Option<i64>>(5).ok().flatten(),
                "created_at": row.get::<_, Option<String>>(6).ok().flatten(),
            }))
        },
    );
    drop(conn);

    match result {
        Ok(v) => Ok(Json(v).into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(ref name) = body.name {
        state
            .db
            .execute(
                "UPDATE smart_playlists SET name = ? WHERE id = ?",
                &[name as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref rules) = body.rules {
        state
            .db
            .execute(
                "UPDATE smart_playlists SET rules = ? WHERE id = ?",
                &[&rules.to_string() as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state
            .db
            .execute(
                "UPDATE smart_playlists SET sort_by = ? WHERE id = ?",
                &[sort_by as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        state
            .db
            .execute(
                "UPDATE smart_playlists SET sort_order = ? WHERE id = ?",
                &[sort_order as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref max_tracks) = body.max_tracks {
        state
            .db
            .execute(
                "UPDATE smart_playlists SET max_tracks = ? WHERE id = ?",
                &[max_tracks as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }

    // Return the updated smart playlist as JSON
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let result = conn.query_row(
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at FROM smart_playlists WHERE id = ?",
        rusqlite::params![id],
        |row| {
            let rules_str: String = row.get(2).unwrap_or_else(|_| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(json!({
                "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                "name": row.get::<_, Option<String>>(1).ok().flatten(),
                "rules": rules,
                "sort_by": row.get::<_, Option<String>>(3).ok().flatten(),
                "sort_order": row.get::<_, Option<String>>(4).ok().flatten(),
                "max_tracks": row.get::<_, Option<i64>>(5).ok().flatten(),
                "created_at": row.get::<_, Option<String>>(6).ok().flatten(),
            }))
        },
    );
    drop(conn);

    match result {
        Ok(v) => Ok(Json(v).into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state
        .db
        .execute("DELETE FROM smart_playlists WHERE id = ?", &[&id])
        .ok();
    Json(json!({"deleted": true, "id": id}))
}

/// Build WHERE, ORDER, LIMIT clauses from smart playlist criteria.
fn build_smart_query(
    rules_json: &str,
    sort_by: &str,
    sort_order: &str,
    max_tracks: Option<i64>,
) -> (String, String, String) {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let op = rule
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("contains");
        let value = rule.get("value").and_then(|v| v.as_str()).unwrap_or("");

        let cond = match (field, op) {
            ("genre", "eq") => format!("t.genre = '{}'", value.replace('\'', "''")),
            ("genre", "contains") => format!("t.genre LIKE '%{}%'", value.replace('\'', "''")),
            ("artist", "eq") => format!("ar.name = '{}'", value.replace('\'', "''")),
            ("artist", "contains") => format!("ar.name LIKE '%{}%'", value.replace('\'', "''")),
            ("year", "eq") => format!("t.year = {}", value.parse::<i32>().unwrap_or(0)),
            ("year", "gte") => format!("t.year >= {}", value.parse::<i32>().unwrap_or(0)),
            ("year", "lte") => format!("t.year <= {}", value.parse::<i32>().unwrap_or(0)),
            ("format", "eq") => format!("t.format = '{}'", value.replace('\'', "''")),
            ("sample_rate", "gte") => {
                format!("t.sample_rate >= {}", value.parse::<i32>().unwrap_or(0))
            }
            ("duration_ms", "gte") => {
                format!("t.duration_ms >= {}", value.parse::<i64>().unwrap_or(0))
            }
            ("duration_ms", "lte") => {
                format!("t.duration_ms <= {}", value.parse::<i64>().unwrap_or(0))
            }
            ("title", "contains") => format!("t.title LIKE '%{}%'", value.replace('\'', "''")),
            ("comments", "contains") => {
                format!("t.comments LIKE '%{}%'", value.replace('\'', "''"))
            }
            ("comments", "eq") | ("comments", "equals") => {
                format!("t.comments = '{}'", value.replace('\'', "''"))
            }
            ("comments", "starts_with") => {
                format!("t.comments LIKE '{}%'", value.replace('\'', "''"))
            }
            ("comments", "ends_with") => {
                format!("t.comments LIKE '%{}'", value.replace('\'', "''"))
            }
            ("comments", "is_empty") => "(t.comments IS NULL OR t.comments = '')".to_string(),
            ("comments", "is_not_empty") => {
                "(t.comments IS NOT NULL AND t.comments != '')".to_string()
            }
            ("play_count", "eq") => {
                let n = value.parse::<i64>().unwrap_or(0);
                if n == 0 {
                    "t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL)".into()
                } else {
                    format!(
                        "t.id IN (SELECT track_id FROM listen_history WHERE track_id IS NOT NULL GROUP BY track_id HAVING COUNT(*) = {})",
                        n
                    )
                }
            }
            ("play_count", "gte") => format!(
                "t.id IN (SELECT track_id FROM listen_history WHERE track_id IS NOT NULL GROUP BY track_id HAVING COUNT(*) >= {})",
                value.parse::<i64>().unwrap_or(0)
            ),
            _ => continue,
        };
        conditions.push(cond);
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let order = if sort_by == "random" {
        "ORDER BY RANDOM()".to_string()
    } else {
        format!(
            "ORDER BY {} {}",
            match sort_by {
                "artist" => "ar.name",
                "album" => "al.title",
                "year" => "t.year",
                "duration" => "t.duration_ms",
                "added_at" => "t.id",
                "play_count" => "t.play_count",
                _ => "t.title",
            },
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };

    let limit_clause = max_tracks.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    (where_clause, order, limit_clause)
}

/// Execute a smart query and return track rows as JSON values.
fn execute_smart_track_query(
    state: &AppState,
    where_clause: &str,
    order: &str,
    limit_clause: &str,
) -> Result<Vec<Value>, AppError> {
    let needs_play_count = order.contains("play_count");
    let play_count_join = if needs_play_count {
        "LEFT JOIN (SELECT track_id, COUNT(*) AS play_count FROM listen_history WHERE track_id IS NOT NULL GROUP BY track_id) lh ON t.id = lh.track_id"
    } else {
        ""
    };
    // Replace play_count reference with the computed column (COALESCE for never-played)
    let order = if needs_play_count {
        order.replace("t.play_count", "COALESCE(lh.play_count, 0)")
    } else {
        order.to_string()
    };
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.format, t.genre, t.year, al.id, al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         {} {} {} {}",
        play_count_join, where_clause, order, limit_clause
    );

    let rows = state
        .backend
        .query_many(&sql, &[])
        .map_err(|e| AppError::internal(format!("{e}")))?;
    Ok(rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "format": cols.get(5).and_then(|v| v.as_string()),
                "genre": cols.get(6).and_then(|v| v.as_string()),
                "year": cols.get(7).and_then(|v| v.as_i64()).map(|y| y as i32),
                "album_id": cols.get(8).and_then(|v| v.as_i64()),
                "cover_path": cols.get(9).and_then(|v| v.as_string()),
            })
        })
        .collect())
}

/// Load a smart playlist's criteria from the DB. Returns (rules_json, sort_by, sort_order, max_tracks).
fn load_smart_criteria(
    state: &AppState,
    id: i64,
) -> Result<Option<(String, String, String, Option<i64>)>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    Ok(conn
        .query_row(
            "SELECT rules, sort_by, sort_order, max_tracks FROM smart_playlists WHERE id = ?",
            rusqlite::params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_else(|_| "[]".into()),
                    row.get::<_, String>(1).unwrap_or_else(|_| "title".into()),
                    row.get::<_, String>(2).unwrap_or_else(|_| "asc".into()),
                    row.get::<_, Option<i64>>(3).ok().flatten(),
                ))
            },
        )
        .ok())
}

async fn resolve_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, max_tracks)) = load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let (where_clause, order, limit_clause) =
        build_smart_query(&rules_json, &sort_by, &sort_order, max_tracks);
    let items = execute_smart_track_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!(items)).into_response())
}

async fn smart_collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, max_tracks)) = load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let (where_clause, order, limit_clause) =
        build_smart_query(&rules_json, &sort_by, &sort_order, max_tracks);
    let tracks = execute_smart_track_query(&state, &where_clause, &order, &limit_clause)?;

    // Group tracks by album_id, dedup albums
    let mut seen = std::collections::HashSet::new();
    let mut albums: Vec<Value> = Vec::new();
    for track in &tracks {
        if let Some(album_id) = track.get("album_id").and_then(|v| v.as_i64()) {
            if seen.insert(album_id) {
                albums.push(json!({
                    "album_id": album_id,
                    "album_title": track.get("album_title"),
                    "artist_name": track.get("artist_name"),
                    "cover_path": track.get("cover_path"),
                    "year": track.get("year"),
                }));
            }
        }
    }

    Ok(Json(json!({"albums": albums, "total": albums.len()})).into_response())
}

async fn preview_smart_collection(
    State(state): State<AppState>,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    let rules_json = body.rules.to_string();
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    let (where_clause, order, limit_clause) =
        build_smart_query(&rules_json, sort_by, sort_order, body.max_tracks);
    let items = execute_smart_track_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!({"tracks": items, "total": items.len()})))
}
