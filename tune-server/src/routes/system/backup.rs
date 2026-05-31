use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

pub(super) async fn list_backups() -> Json<Value> {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    let items = tune_core::db_backup::list_backups(&db_path);
    Json(json!(items))
}

pub(super) async fn create_backup(State(state): State<AppState>) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (StatusCode::BAD_REQUEST, "cannot backup in-memory database").into_response();
    }

    state
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .ok();

    match tune_core::db_backup::create_backup(&db_path) {
        Some(info) => Json(json!(info)).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "backup failed").into_response(),
    }
}

pub(super) async fn restore_backup(
    State(_state): State<AppState>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> impl IntoResponse {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return (
            StatusCode::BAD_REQUEST,
            "cannot restore to in-memory database",
        )
            .into_response();
    }

    if tune_core::db_backup::restore_backup(&db_path, &filename) {
        Json(json!({
            "restored": true,
            "filename": filename,
            "message": "restart required to apply",
        }))
        .into_response()
    } else {
        (StatusCode::NOT_FOUND, "backup not found or restore failed").into_response()
    }
}
