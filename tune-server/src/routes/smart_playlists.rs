use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

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
struct UpdateSmartPlaylist {
    name: Option<String>,
    rules: Option<Value>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    max_tracks: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_smart_playlists).post(create_smart_playlist))
        .route("/{id}", get(get_smart_playlist).put(update_smart_playlist).delete(delete_smart_playlist))
        .route("/{id}/tracks", get(resolve_tracks))
}

async fn list_smart_playlists(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
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
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!({ "items": items, "total": items.len() }))
}

async fn create_smart_playlist(
    State(state): State<AppState>,
    Json(body): Json<CreateSmartPlaylist>,
) -> impl IntoResponse {
    let rules_json = body.rules.to_string();
    match state.db.execute(
        "INSERT INTO smart_playlists (name, rules, sort_by, sort_order, max_tracks) VALUES (?, ?, ?, ?, ?)",
        &[
            &body.name as &dyn rusqlite::types::ToSql,
            &rules_json,
            &body.sort_by.unwrap_or_else(|| "title".into()),
            &body.sort_order.unwrap_or_else(|| "asc".into()),
            &body.max_tracks,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
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
        Ok(v) => Json(v).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn update_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateSmartPlaylist>,
) -> impl IntoResponse {
    if let Some(ref name) = body.name {
        state.db.execute(
            "UPDATE smart_playlists SET name = ? WHERE id = ?",
            &[name as &dyn rusqlite::types::ToSql, &id],
        ).ok();
    }
    if let Some(ref rules) = body.rules {
        state.db.execute(
            "UPDATE smart_playlists SET rules = ? WHERE id = ?",
            &[&rules.to_string() as &dyn rusqlite::types::ToSql, &id],
        ).ok();
    }
    if let Some(ref sort_by) = body.sort_by {
        state.db.execute(
            "UPDATE smart_playlists SET sort_by = ? WHERE id = ?",
            &[sort_by as &dyn rusqlite::types::ToSql, &id],
        ).ok();
    }
    if let Some(ref max_tracks) = body.max_tracks {
        state.db.execute(
            "UPDATE smart_playlists SET max_tracks = ? WHERE id = ?",
            &[max_tracks as &dyn rusqlite::types::ToSql, &id],
        ).ok();
    }
    StatusCode::NO_CONTENT
}

async fn delete_smart_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state.db.execute("DELETE FROM smart_playlists WHERE id = ?", &[&id]).ok();
    StatusCode::NO_CONTENT
}

async fn resolve_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
    let rules_result = conn.query_row(
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
    );
    drop(conn);

    let (rules_json, sort_by, sort_order, max_tracks) = match rules_result {
        Ok(v) => v,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let rules: Vec<Value> = serde_json::from_str(&rules_json).unwrap_or_default();

    let mut conditions = Vec::new();
    for rule in &rules {
        let field = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let op = rule.get("op").and_then(|v| v.as_str()).unwrap_or("contains");
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
            ("sample_rate", "gte") => format!("t.sample_rate >= {}", value.parse::<i32>().unwrap_or(0)),
            ("duration_ms", "gte") => format!("t.duration_ms >= {}", value.parse::<i64>().unwrap_or(0)),
            ("duration_ms", "lte") => format!("t.duration_ms <= {}", value.parse::<i64>().unwrap_or(0)),
            ("title", "contains") => format!("t.title LIKE '%{}%'", value.replace('\'', "''")),
            _ => continue,
        };
        conditions.push(cond);
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let order = format!(
        "ORDER BY {} {}",
        match sort_by.as_str() {
            "artist" => "ar.name",
            "album" => "al.title",
            "year" => "t.year",
            "duration" => "t.duration_ms",
            _ => "t.title",
        },
        if sort_order == "desc" { "DESC" } else { "ASC" }
    );

    let limit_clause = max_tracks
        .map(|n| format!("LIMIT {n}"))
        .unwrap_or_default();

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.format, t.genre, t.year FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id {} {} {}",
        where_clause, order, limit_clause
    );

    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "format": row.get::<_, Option<String>>(5).ok().flatten(),
                    "genre": row.get::<_, Option<String>>(6).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(7).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);

    Json(json!({ "items": items, "total": items.len() })).into_response()
}
