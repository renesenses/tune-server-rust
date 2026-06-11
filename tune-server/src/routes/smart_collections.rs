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
struct CreateCollection {
    name: String,
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UpdateCollection {
    name: Option<String>,
    rules: Option<Value>,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
    description: Option<String>,
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
struct PreviewRequest {
    rules: Value,
    match_mode: Option<String>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_limit: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_collections).post(create_collection))
        .route(
            "/{id}",
            get(get_collection)
                .put(update_collection)
                .delete(delete_collection),
        )
        .route("/{id}/albums", get(resolve_albums))
        .route("/preview", post(preview_albums))
}

async fn list_collections(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
             description, icon, color, created_at \
             FROM smart_collections ORDER BY name",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                let rules_str: String = row.get(2).unwrap_or_else(|_| "[]".into());
                let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "name": row.get::<_, Option<String>>(1).ok().flatten(),
                    "rules": rules,
                    "match_mode": row.get::<_, Option<String>>(3).ok().flatten().unwrap_or_else(|| "all".into()),
                    "sort_by": row.get::<_, Option<String>>(4).ok().flatten(),
                    "sort_order": row.get::<_, Option<String>>(5).ok().flatten().unwrap_or_else(|| "asc".into()),
                    "max_limit": row.get::<_, Option<i64>>(6).ok().flatten(),
                    "description": row.get::<_, Option<String>>(7).ok().flatten(),
                    "icon": row.get::<_, Option<String>>(8).ok().flatten(),
                    "color": row.get::<_, Option<String>>(9).ok().flatten(),
                    "created_at": row.get::<_, Option<String>>(10).ok().flatten(),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}

async fn create_collection(
    State(state): State<AppState>,
    Json(body): Json<CreateCollection>,
) -> Result<impl IntoResponse, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.clone().unwrap_or_else(|| "all".into());
    let sort_by = body.sort_by.clone();
    let sort_order = body.sort_order.clone().unwrap_or_else(|| "asc".into());

    let result = {
        let conn = state
            .db
            .connection()
            .lock()
            .map_err(|e| AppError::internal(format!("{e}")))?;
        conn.execute(
            "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order, max_limit, description, icon, color) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                body.name, rules_json, match_mode, sort_by, sort_order,
                body.max_limit, body.description, body.icon, body.color
            ],
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
                "match_mode": match_mode,
                "sort_by": sort_by,
                "sort_order": sort_order,
                "max_limit": body.max_limit,
                "description": body.description,
                "icon": body.icon,
                "color": body.color,
            });
            Ok((StatusCode::CREATED, Json(created)).into_response())
        }
        Err(e) => Err(AppError::internal(e)),
    }
}

async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let result = conn.query_row(
        "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = ?",
        rusqlite::params![id],
        |row| {
            let rules_str: String = row.get(2).unwrap_or_else(|_| "[]".into());
            let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
            Ok(json!({
                "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                "name": row.get::<_, Option<String>>(1).ok().flatten(),
                "rules": rules,
                "match_mode": row.get::<_, Option<String>>(3).ok().flatten().unwrap_or_else(|| "all".into()),
                "sort_by": row.get::<_, Option<String>>(4).ok().flatten(),
                "sort_order": row.get::<_, Option<String>>(5).ok().flatten().unwrap_or_else(|| "asc".into()),
                "max_limit": row.get::<_, Option<i64>>(6).ok().flatten(),
                "description": row.get::<_, Option<String>>(7).ok().flatten(),
                "icon": row.get::<_, Option<String>>(8).ok().flatten(),
                "color": row.get::<_, Option<String>>(9).ok().flatten(),
                "created_at": row.get::<_, Option<String>>(10).ok().flatten(),
            }))
        },
    );
    drop(conn);

    match result {
        Ok(v) => Ok(Json(v).into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollection>,
) -> impl IntoResponse {
    if let Some(ref name) = body.name {
        state
            .db
            .execute(
                "UPDATE smart_collections SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[name as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref rules) = body.rules {
        state
            .db
            .execute(
                "UPDATE smart_collections SET rules = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[&rules.to_string() as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        state
            .db
            .execute(
                "UPDATE smart_collections SET match_mode = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[match_mode as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state
            .db
            .execute(
                "UPDATE smart_collections SET sort_by = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[sort_by as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        state
            .db
            .execute(
                "UPDATE smart_collections SET sort_order = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[sort_order as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref max_limit) = body.max_limit {
        state
            .db
            .execute(
                "UPDATE smart_collections SET max_limit = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[max_limit as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref description) = body.description {
        state
            .db
            .execute(
                "UPDATE smart_collections SET description = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[description as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref icon) = body.icon {
        state
            .db
            .execute(
                "UPDATE smart_collections SET icon = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[icon as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    if let Some(ref color) = body.color {
        state
            .db
            .execute(
                "UPDATE smart_collections SET color = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                &[color as &dyn rusqlite::types::ToSql, &id],
            )
            .ok();
    }
    StatusCode::NO_CONTENT
}

async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state
        .db
        .execute("DELETE FROM smart_collections WHERE id = ?", &[&id])
        .ok();
    StatusCode::NO_CONTENT
}

/// Build WHERE, ORDER, LIMIT clauses from smart collection criteria (album-level).
fn build_album_query(
    rules_json: &str,
    match_mode: &str,
    sort_by: &str,
    sort_order: &str,
    max_limit: Option<i64>,
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
            ("album", "eq") => format!("al.title = '{}'", value.replace('\'', "''")),
            ("album", "contains") => format!("al.title LIKE '%{}%'", value.replace('\'', "''")),
            ("year", "eq") => format!("al.year = {}", value.parse::<i32>().unwrap_or(0)),
            ("year", "gte") => format!("al.year >= {}", value.parse::<i32>().unwrap_or(0)),
            ("year", "lte") => format!("al.year <= {}", value.parse::<i32>().unwrap_or(0)),
            ("year", "between") => {
                let parts: Vec<&str> = value.splitn(2, ',').collect();
                if parts.len() == 2 {
                    format!(
                        "al.year BETWEEN {} AND {}",
                        parts[0].trim().parse::<i32>().unwrap_or(0),
                        parts[1].trim().parse::<i32>().unwrap_or(9999)
                    )
                } else {
                    format!("al.year = {}", value.parse::<i32>().unwrap_or(0))
                }
            }
            ("format", "eq") => format!("t.format = '{}'", value.replace('\'', "''")),
            ("sample_rate", "gte") => {
                format!("t.sample_rate >= {}", value.parse::<i32>().unwrap_or(0))
            }
            ("bit_depth", "gte") => {
                format!("t.bit_depth >= {}", value.parse::<i32>().unwrap_or(0))
            }
            ("label", "eq") => format!("al.label = '{}'", value.replace('\'', "''")),
            ("label", "contains") => format!("al.label LIKE '%{}%'", value.replace('\'', "''")),
            _ => continue,
        };
        conditions.push(cond);
    }

    let joiner = if match_mode == "any" { " OR " } else { " AND " };

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
                "album" | "title" => "al.title",
                "year" => "al.year",
                "added_at" => "al.id",
                "track_count" => "track_count",
                _ => "al.title",
            },
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };

    let limit_clause = max_limit.map(|n| format!("LIMIT {n}")).unwrap_or_default();

    (where_clause, order, limit_clause)
}

/// Execute a smart album query and return album rows as JSON values.
fn execute_album_query(
    state: &AppState,
    where_clause: &str,
    order: &str,
    limit_clause: &str,
) -> Result<Vec<Value>, AppError> {
    let sql = format!(
        "SELECT al.id, al.title, ar.name, al.year, al.cover_path, al.genre, \
         COUNT(t.id) AS track_count \
         FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id \
         {} \
         GROUP BY al.id \
         {} {}",
        where_clause, order, limit_clause
    );

    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    Ok(conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(3).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(4).ok().flatten(),
                    "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                    "track_count": row.get::<_, i64>(6).unwrap_or(0),
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default())
}

/// Load a smart collection's criteria from the DB.
fn load_collection_criteria(
    state: &AppState,
    id: i64,
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    Ok(conn
        .query_row(
            "SELECT rules, match_mode, sort_by, sort_order, max_limit FROM smart_collections WHERE id = ?",
            rusqlite::params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_else(|_| "[]".into()),
                    row.get::<_, String>(1).unwrap_or_else(|_| "all".into()),
                    row.get::<_, String>(2).unwrap_or_else(|_| "title".into()),
                    row.get::<_, String>(3).unwrap_or_else(|_| "asc".into()),
                    row.get::<_, Option<i64>>(4).ok().flatten(),
                ))
            },
        )
        .ok())
}

async fn resolve_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let Some((rules_json, match_mode, sort_by, sort_order, max_limit)) =
        load_collection_criteria(&state, id)?
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };

    let (where_clause, order, limit_clause) =
        build_album_query(&rules_json, &match_mode, &sort_by, &sort_order, max_limit);
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!({"albums": albums, "total": albums.len()})).into_response())
}

async fn preview_albums(
    State(state): State<AppState>,
    Json(body): Json<PreviewRequest>,
) -> Result<Json<Value>, AppError> {
    let rules_json = body.rules.to_string();
    let match_mode = body.match_mode.as_deref().unwrap_or("all");
    let sort_by = body.sort_by.as_deref().unwrap_or("title");
    let sort_order = body.sort_order.as_deref().unwrap_or("asc");

    let (where_clause, order, limit_clause) =
        build_album_query(&rules_json, match_mode, sort_by, sort_order, body.max_limit);
    let albums = execute_album_query(&state, &where_clause, &order, &limit_clause)?;

    Ok(Json(json!({"albums": albums, "total": albums.len()})))
}
