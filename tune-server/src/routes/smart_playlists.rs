use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::Engine;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateSmartPlaylist {
    name: String,
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateSmartPlaylist {
    name: Option<String>,
    rules: Option<Value>,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    match_mode: Option<String>,
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
    let rows = state
        .backend
        .query_many(
            "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists ORDER BY name",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "match_mode": cols.get(7).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

async fn create_smart_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.clone().unwrap_or_else(|| "all".into());
    let sort_by = body.sort_by.clone().unwrap_or_else(|| "title".into());
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    let sql = if state.backend.engine() == Engine::Postgres {
        "INSERT INTO smart_playlists (name, rules, match_mode, sort_by, sort_order, max_tracks) VALUES ($1, $2, $3, $4, $5, $6)"
    } else {
        "INSERT INTO smart_playlists (name, rules, match_mode, sort_by, sort_order, max_tracks) VALUES (?, ?, ?, ?, ?, ?)"
    };

    let result = state
        .backend
        .execute(
            sql,
            &[
                &body.name as &dyn ToSqlValue,
                &rules_json as &dyn ToSqlValue,
                &match_mode as &dyn ToSqlValue,
                &sort_by as &dyn ToSqlValue,
                &sort_order as &dyn ToSqlValue,
                &body.max_tracks as &dyn ToSqlValue,
            ],
        )
        .map(|_| state.backend.last_insert_rowid())
        .map_err(|e| AppError::internal(e));

    match result {
        Ok(id) => {
            let created = json!({
                "id": id,
                "name": body.name,
                "rules": body.rules,
                "match_mode": match_mode,
                "sort_by": sort_by,
                "sort_order": sort_order,
                "max_tracks": body.max_tracks,
            });
            Ok((StatusCode::CREATED, Json(created)).into_response())
        }
        Err(e) => Err(e),
    }
}

async fn get_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let sql = if state.backend.engine() == Engine::Postgres {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;

    match result {
        Some(cols) => {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(Json(json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "match_mode": cols.get(7).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            }))
            .into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateSmartPlaylist>,
) -> Result<impl IntoResponse, AppError> {
    let pg = state.backend.engine() == Engine::Postgres;

    if let Some(ref name) = body.name {
        let sql = if pg {
            "UPDATE smart_playlists SET name = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET name = ? WHERE id = ?"
        };
        state
            .backend
            .execute(sql, &[name as &dyn ToSqlValue, &id as &dyn ToSqlValue])
            .ok();
    }
    if let Some(ref rules) = body.rules {
        let rules_str = rules.to_string();
        let sql = if pg {
            "UPDATE smart_playlists SET rules = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET rules = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[&rules_str as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        let sql = if pg {
            "UPDATE smart_playlists SET sort_by = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET sort_by = ? WHERE id = ?"
        };
        state
            .backend
            .execute(sql, &[sort_by as &dyn ToSqlValue, &id as &dyn ToSqlValue])
            .ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        let sql = if pg {
            "UPDATE smart_playlists SET sort_order = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET sort_order = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[sort_order as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref max_tracks) = body.max_tracks {
        let sql = if pg {
            "UPDATE smart_playlists SET max_tracks = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET max_tracks = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[max_tracks as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        let sql = if pg {
            "UPDATE smart_playlists SET match_mode = $1 WHERE id = $2"
        } else {
            "UPDATE smart_playlists SET match_mode = ? WHERE id = ?"
        };
        state
            .backend
            .execute(
                sql,
                &[match_mode as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
            .ok();
    }

    // Return the updated smart playlist as JSON
    let sql = if pg {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT id, name, rules, sort_by, sort_order, max_tracks, created_at, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;

    match result {
        Some(cols) => {
            let rules_str = cols
                .get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(Json(json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "name": cols.get(1).and_then(|v| v.as_string()),
                "rules": rules,
                "sort_by": cols.get(3).and_then(|v| v.as_string()),
                "sort_order": cols.get(4).and_then(|v| v.as_string()),
                "max_tracks": cols.get(5).and_then(|v| v.as_i64()),
                "created_at": cols.get(6).and_then(|v| v.as_string()),
            }))
            .into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let sql = if state.backend.engine() == Engine::Postgres {
        "DELETE FROM smart_playlists WHERE id = $1"
    } else {
        "DELETE FROM smart_playlists WHERE id = ?"
    };
    state.backend.execute(sql, &[&id as &dyn ToSqlValue]).ok();
    Json(json!({"deleted": true, "id": id}))
}

/// Build WHERE, ORDER, LIMIT clauses from smart playlist criteria.
fn build_smart_query(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_tracks: Option<i64>,
) -> (String, String, String) {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let joiner = if match_mode == "any" { " OR " } else { " AND " };

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let raw_op = rule
            .get("op")
            .and_then(|v| v.as_str())
            .unwrap_or("contains");
        let op = match raw_op {
            "greater_than" => "gte",
            "less_than" => "lte",
            "equals" => "eq",
            "not_equals" => "neq",
            other => other,
        };
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
            ("sample_rate", "gte") | ("sample_rate", "gt") => {
                format!("t.sample_rate > {}", value.parse::<i32>().unwrap_or(0))
            }
            ("sample_rate", "lte") | ("sample_rate", "lt") => {
                format!("t.sample_rate < {}", value.parse::<i32>().unwrap_or(0))
            }
            ("sample_rate", "eq") => {
                format!("t.sample_rate = {}", value.parse::<i32>().unwrap_or(0))
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
        format!("WHERE {}", conditions.join(joiner))
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
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let sql = if state.backend.engine() == Engine::Postgres {
        "SELECT rules, sort_by, sort_order, max_tracks, match_mode FROM smart_playlists WHERE id = $1"
    } else {
        "SELECT rules, sort_by, sort_order, max_tracks, match_mode FROM smart_playlists WHERE id = ?"
    };
    let result = state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .map_err(|e| AppError::internal(e))?;
    Ok(result.map(|cols| {
        (
            cols.get(0)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            cols.get(1)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "title".into()),
            cols.get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "asc".into()),
            cols.get(4)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "all".into()),
            cols.get(3).and_then(|v| v.as_i64()),
        )
    }))
}

async fn resolve_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, match_mode, max_tracks)) =
        load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let (where_clause, order, limit_clause) =
        build_smart_query(&rules_json, &match_mode, &sort_by, &sort_order, max_tracks);
    let items = execute_smart_track_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!(items)).into_response())
}

async fn smart_collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, sort_by, sort_order, match_mode, max_tracks)) =
        load_smart_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let (where_clause, order, limit_clause) =
        build_smart_query(&rules_json, &match_mode, &sort_by, &sort_order, max_tracks);
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
    let match_mode = body.match_mode.as_deref().unwrap_or("all");
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    let (where_clause, order, limit_clause) = build_smart_query(
        &rules_json,
        match_mode,
        sort_by,
        sort_order,
        body.max_tracks,
    );
    let items = execute_smart_track_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!({"tracks": items, "total": items.len()})))
}
