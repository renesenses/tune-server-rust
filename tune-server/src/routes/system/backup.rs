use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

#[allow(unused_imports)]
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

pub(super) async fn create_encrypted_backup(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let password = match body["password"].as_str() {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "password required"})),
            )
                .into_response();
        }
    };

    let db_path = state.config.db_path.clone();
    let backup = tune_core::db_backup::create_backup(&db_path);
    let Some(info) = backup else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "backup creation failed"})),
        )
            .into_response();
    };

    let backup_path = format!("backups/{}", info.filename);
    let data = match std::fs::read(&backup_path) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
                .into_response();
        }
    };

    let encrypted = tune_core::db_backup::encrypt_backup(&data, &password);
    let enc_filename = format!("{}.enc", info.filename);
    let enc_path = format!("backups/{enc_filename}");
    if let Err(e) = std::fs::write(&enc_path, &encrypted) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        )
            .into_response();
    }

    Json(json!({
        "filename": enc_filename,
        "size": encrypted.len(),
        "encrypted": true,
    }))
    .into_response()
}
