use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::backend::ToSqlValue;

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

/// Decode a row from `smart_collections` into a JSON object.
/// Column order: id(0), name(1), rules(2), match_mode(3), sort_by(4),
/// sort_order(5), max_limit(6), description(7), icon(8), color(9), created_at(10).
fn decode_collection_row(r: &[tune_core::db::backend::SqlValue]) -> Value {
    let rules_str = r
        .get(2)
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| "[]".into());
    let rules = serde_json::from_str::<Value>(&rules_str).unwrap_or(json!([]));
    json!({
        "id": r.get(0).and_then(|v| v.as_i64()),
        "name": r.get(1).and_then(|v| v.as_string()),
        "rules": rules,
        "match_mode": r.get(3).and_then(|v| v.as_string()).unwrap_or_else(|| "all".into()),
        "sort_by": r.get(4).and_then(|v| v.as_string()),
        "sort_order": r.get(5).and_then(|v| v.as_string()).unwrap_or_else(|| "asc".into()),
        "max_limit": r.get(6).and_then(|v| v.as_i64()),
        "description": r.get(7).and_then(|v| v.as_string()),
        "icon": r.get(8).and_then(|v| v.as_string()),
        "color": r.get(9).and_then(|v| v.as_string()),
        "created_at": r.get(10).and_then(|v| v.as_string()),
    })
}

async fn list_collections(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections ORDER BY name",
            &[],
        )
        .map_err(AppError::internal)?;

    let items: Vec<Value> = rows.iter().map(|r| decode_collection_row(r)).collect();
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

    state
        .backend
        .execute(
            "INSERT INTO smart_collections \
         (name, rules, match_mode, sort_by, sort_order, max_limit, description, icon, color) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &body.name as &dyn ToSqlValue,
                &rules_json as &dyn ToSqlValue,
                &match_mode as &dyn ToSqlValue,
                &sort_by as &dyn ToSqlValue,
                &sort_order as &dyn ToSqlValue,
                &body.max_limit as &dyn ToSqlValue,
                &body.description as &dyn ToSqlValue,
                &body.icon as &dyn ToSqlValue,
                &body.color as &dyn ToSqlValue,
            ],
        )
        .map_err(AppError::internal)?;

    let id = state.backend.last_insert_rowid();

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

async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn update_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollection>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(ref name) = body.name {
        state.backend.execute(
            "UPDATE smart_collections SET name = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref rules) = body.rules {
        let rules_json = rules.to_string();
        state.backend.execute(
            "UPDATE smart_collections SET rules = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[&rules_json as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref match_mode) = body.match_mode {
        state.backend.execute(
            "UPDATE smart_collections SET match_mode = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[match_mode as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state.backend.execute(
            "UPDATE smart_collections SET sort_by = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_by as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref sort_order) = body.sort_order {
        state.backend.execute(
            "UPDATE smart_collections SET sort_order = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[sort_order as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref max_limit) = body.max_limit {
        state.backend.execute(
            "UPDATE smart_collections SET max_limit = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[max_limit as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref description) = body.description {
        state.backend.execute(
            "UPDATE smart_collections SET description = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[description as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref icon) = body.icon {
        state.backend.execute(
            "UPDATE smart_collections SET icon = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[icon as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }
    if let Some(ref color) = body.color {
        state.backend.execute(
            "UPDATE smart_collections SET color = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2",
            &[color as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ).ok();
    }

    // Return the updated collection as JSON
    let row = state
        .backend
        .query_one(
            "SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit, \
         description, icon, color, created_at \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    match row {
        Some(r) => Ok(Json(decode_collection_row(&r)).into_response()),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state
        .backend
        .execute(
            "DELETE FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    Json(json!({"deleted": true, "id": id}))
}

fn resolve_timestamp_sql(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("now-") {
        let days: i64 = rest.trim_end_matches('d').parse().unwrap_or(30);
        return format!("DATETIME('now', '-{days} days')");
    }
    format!("'{}'", input.replace('\'', "''"))
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
        let raw_op = rule
            .get("operator")
            .or_else(|| rule.get("op"))
            .and_then(|v| v.as_str())
            .unwrap_or("contains");
        let op = match raw_op {
            "=" | "eq" | "equals" => "=",
            "!=" | "ne" | "not_equals" => "!=",
            ">=" | "gte" | "greater_than" | "greater_equal" => ">=",
            ">" | "gt" => ">",
            "<=" | "lte" | "less_than" | "less_equal" => "<=",
            "<" | "lt" => "<",
            other => other,
        };
        let value_raw = rule.get("value");
        let value = value_raw
            .map(|v| match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => v.to_string(),
            })
            .unwrap_or_default();
        let esc = value.replace('\'', "''");

        // --- credit rules use a subquery, handle separately ---
        if field == "credit" {
            if op == "has" {
                if let Some(obj) = value_raw.and_then(|v| v.as_object()) {
                    let mut sub_conds = Vec::new();
                    if let Some(role) = obj
                        .get("role")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.role) LIKE LOWER('%{}%')",
                            role.replace('\'', "''")
                        ));
                    }
                    if let Some(artist) = obj
                        .get("artist_name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.artist_name) LIKE LOWER('%{}%')",
                            artist.replace('\'', "''")
                        ));
                    }
                    if let Some(instr) = obj
                        .get("instrument")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        sub_conds.push(format!(
                            "LOWER(tc.instrument) LIKE LOWER('%{}%')",
                            instr.replace('\'', "''")
                        ));
                    }
                    if !sub_conds.is_empty() {
                        conditions.push(format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             JOIN track_credits tc ON tc.track_id = t2.id WHERE {})",
                            sub_conds.join(" AND ")
                        ));
                    }
                }
            }
            continue;
        }

        // --- added_at / last_played_at use timestamp logic ---
        if field == "added_at" {
            let ts = resolve_timestamp_sql(&value);
            let cond = match op {
                ">" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') > {ts})"
                ),
                "<" => format!(
                    "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                     WHERE DATETIME(t2.file_mtime, 'unixepoch') < {ts})"
                ),
                "between" => {
                    if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                        let lo = arr.first().and_then(|v| v.as_str()).unwrap_or("2000-01-01");
                        let hi = arr.get(1).and_then(|v| v.as_str()).unwrap_or("2099-01-01");
                        let lo_sql = resolve_timestamp_sql(lo);
                        let hi_sql = resolve_timestamp_sql(hi);
                        format!(
                            "al.id IN (SELECT DISTINCT t2.album_id FROM tracks t2 \
                             WHERE DATETIME(t2.file_mtime, 'unixepoch') BETWEEN {lo_sql} AND {hi_sql})"
                        )
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        if field == "play_count" || field == "last_played_at" {
            let int_v = value.parse::<i64>().unwrap_or(0);
            let cond = match (field, op) {
                ("play_count", "=" | "==") if int_v == 0 => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                ("play_count", "=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) = {int_v})"
                ),
                ("play_count", ">=") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("play_count", ">") => format!(
                    "al.id IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) > {int_v})"
                ),
                ("play_count", "<") => format!(
                    "al.id NOT IN (SELECT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id \
                     GROUP BY t3.album_id HAVING COUNT(*) >= {int_v})"
                ),
                ("last_played_at", ">") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         WHERE lh.listened_at > {ts} GROUP BY t3.album_id)"
                    )
                }
                ("last_played_at", "<") => {
                    let ts = resolve_timestamp_sql(&value);
                    format!(
                        "al.id IN (SELECT t3.album_id FROM tracks t3 \
                         JOIN listen_history lh ON lh.track_id = t3.id \
                         GROUP BY t3.album_id HAVING MAX(lh.listened_at) < {ts})"
                    )
                }
                ("last_played_at", "is_null") => format!(
                    "al.id NOT IN (SELECT DISTINCT t3.album_id FROM tracks t3 \
                     JOIN listen_history lh ON lh.track_id = t3.id)"
                ),
                _ => continue,
            };
            conditions.push(cond);
            continue;
        }

        let col = match field {
            "genre" => "t.genre",
            "artist" | "artist_name" => "ar.name",
            "album" | "album_title" | "title" => "al.title",
            "composer" => "t.composer",
            "label" => "al.label",
            "format" => "t.format",
            "source" => "t.source",
            "cover_path" => "al.cover_path",
            "year" => "CAST(al.year AS INTEGER)",
            "sample_rate" => "CAST(t.sample_rate AS INTEGER)",
            "bit_depth" => "CAST(t.bit_depth AS INTEGER)",
            "track_count" => "al.track_count",
            "duration" => "CAST(t.duration_ms AS INTEGER)",
            "track_number" => "CAST(t.track_number AS INTEGER)",
            "disc_number" => "CAST(t.disc_number AS INTEGER)",
            "bpm" => "CAST(t.bpm AS INTEGER)",
            "rating" => "CAST(t.rating AS INTEGER)",
            _ => continue,
        };

        let is_text = matches!(
            field,
            "genre"
                | "artist"
                | "artist_name"
                | "album"
                | "album_title"
                | "title"
                | "composer"
                | "label"
                | "format"
                | "source"
        );
        let int_val = || value.parse::<i64>().unwrap_or(0);

        let cond = match op {
            "=" if is_text => format!("LOWER({col}) = LOWER('{esc}')"),
            "!=" if is_text => format!("LOWER({col}) != LOWER('{esc}')"),
            "contains" => format!("LOWER({col}) LIKE LOWER('%{esc}%')"),
            "starts_with" => format!("LOWER({col}) LIKE LOWER('{esc}%')"),
            "is_null" => format!("({col} IS NULL OR {col} = '')"),
            "is_not_null" => format!("({col} IS NOT NULL AND {col} != '')"),
            "=" => format!("{col} = {}", int_val()),
            "!=" => format!("{col} != {}", int_val()),
            ">=" => format!("{col} >= {}", int_val()),
            ">" => format!("{col} > {}", int_val()),
            "<=" => format!("{col} <= {}", int_val()),
            "<" => format!("{col} < {}", int_val()),
            "between" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let lo = arr.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    let hi = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(i64::MAX);
                    format!("{col} BETWEEN {lo} AND {hi}")
                } else {
                    let parts: Vec<&str> = value.splitn(2, ',').collect();
                    if parts.len() == 2 {
                        let lo = parts[0].trim().parse::<i64>().unwrap_or(0);
                        let hi = parts[1].trim().parse::<i64>().unwrap_or(i64::MAX);
                        format!("{col} BETWEEN {lo} AND {hi}")
                    } else {
                        format!("{col} = {}", int_val())
                    }
                }
            }
            "in" => {
                if let Some(arr) = value_raw.and_then(|v| v.as_array()) {
                    let items: Vec<String> = arr
                        .iter()
                        .map(|v| {
                            if let Some(s) = v.as_str() {
                                format!("'{}'", s.replace('\'', "''"))
                            } else {
                                v.to_string()
                            }
                        })
                        .collect();
                    format!("{col} IN ({})", items.join(","))
                } else {
                    let items: Vec<String> = value
                        .split(',')
                        .map(|s| format!("'{}'", s.trim().replace('\'', "''")))
                        .collect();
                    format!("LOWER({col}) IN ({})", items.join(","))
                }
            }
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
                "artist" | "artist_name" => "ar.name",
                "album" | "title" => "al.title",
                "year" => "al.year",
                "added_at" => "al.id",
                "track_count" => "track_count",
                "sample_rate" => "t.sample_rate",
                "label" => "al.label",
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
         GROUP BY al.id, al.title, ar.name, al.year, al.cover_path, al.genre \
         {} {}",
        where_clause, order, limit_clause
    );
    tracing::debug!(sql = %sql, "smart_collection_album_query");

    let rows = state
        .backend
        .query_many(&sql, &[])
        .map_err(AppError::internal)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "year": r.get(3).and_then(|v| v.as_i64()),
                "cover_path": r.get(4).and_then(|v| v.as_string()),
                "genre": r.get(5).and_then(|v| v.as_string()),
                "track_count": r.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

/// Load a smart collection's criteria from the DB.
fn load_collection_criteria(
    state: &AppState,
    id: i64,
) -> Result<Option<(String, String, String, String, Option<i64>)>, AppError> {
    let row = state
        .backend
        .query_one(
            "SELECT rules, match_mode, sort_by, sort_order, max_limit \
         FROM smart_collections WHERE id = $1",
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;

    Ok(row.map(|r| {
        (
            r.get(0)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "[]".into()),
            r.get(1)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "all".into()),
            r.get(2)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "title".into()),
            r.get(3)
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "asc".into()),
            r.get(4).and_then(|v| v.as_i64()),
        )
    }))
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
