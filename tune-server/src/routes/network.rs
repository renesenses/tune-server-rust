use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateMount {
    mount_type: Option<String>,
    server: String,
    share: String,
    mount_path: String,
    username: Option<String>,
    password: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mounts", get(list_mounts).post(create_mount))
        .route("/mounts/{id}", axum::routing::delete(delete_mount))
        .route("/media-servers", get(list_media_servers))
}

async fn list_mounts(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts ORDER BY id")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "mount_type": row.get::<_, Option<String>>(1).ok().flatten(),
                    "server": row.get::<_, Option<String>>(2).ok().flatten(),
                    "share": row.get::<_, Option<String>>(3).ok().flatten(),
                    "mount_path": row.get::<_, Option<String>>(4).ok().flatten(),
                    "username": row.get::<_, Option<String>>(5).ok().flatten(),
                    "active": row.get::<_, i32>(6).unwrap_or(1) != 0,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

async fn create_mount(
    State(state): State<AppState>,
    Json(body): Json<CreateMount>,
) -> impl IntoResponse {
    match state.db.execute(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            &body.mount_type.unwrap_or_else(|| "smb".into()) as &dyn rusqlite::types::ToSql,
            &body.server,
            &body.share,
            &body.mount_path,
            &body.username,
            &body.password,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_mount(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state.db.execute("DELETE FROM network_mounts WHERE id = ?", &[&id]).ok();
    StatusCode::NO_CONTENT
}

async fn list_media_servers() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "UPnP media server discovery pending",
    }))
}
