use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::migrations;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

pub(super) async fn database_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let version = migrations::current_version(&state.db).unwrap_or(0);
    let latest = migrations::latest_version();
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let (artists, albums, tracks): (i64, i64, i64) = conn
        .query_row(
            "SELECT \
             (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
             (SELECT COUNT(*) FROM albums), \
             (SELECT COUNT(*) FROM tracks)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap_or((0, 0, 0));
    drop(conn);

    Ok(Json(json!({
        "engine": "sqlite",
        "migration_version": version,
        "latest_version": latest,
        "up_to_date": version >= latest,
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
    })))
}

pub(super) async fn database_optimize(State(state): State<AppState>) -> impl IntoResponse {
    let start = Instant::now();
    match state.db.execute_batch("PRAGMA optimize; VACUUM; ANALYZE;") {
        Ok(_) => {
            let ms = start.elapsed().as_millis();
            Json(json!({ "status": "ok", "duration_ms": ms })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(super) async fn export_database(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());
    if db_path == ":memory:" {
        return Ok((StatusCode::BAD_REQUEST, "cannot export in-memory database").into_response());
    }

    state
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .ok();

    match tokio::fs::read(&db_path).await {
        Ok(bytes) => {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/x-sqlite3"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str("attachment; filename=\"tune_server.db\"")
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, bytes).into_response())
        }
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("export failed: {e}"),
        )
            .into_response()),
    }
}

pub(super) async fn database_import(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse {
    let mut file_bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "database" {
            file_bytes = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(bytes) = file_bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no file provided"})),
        )
            .into_response();
    };

    // Write to a unique temp file (safe for concurrent imports)
    let tmp_path = std::env::temp_dir().join(format!("tune_import_{}.db", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write failed: {e}")})),
        )
            .into_response();
    }

    // Open the imported DB and count rows
    let import_db = match rusqlite::Connection::open_with_flags(
        &tmp_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("not a valid SQLite file: {e}")})),
            )
                .into_response();
        }
    };

    let track_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
        .unwrap_or(0);
    let album_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))
        .unwrap_or(0);
    let artist_count: i64 = import_db
        .query_row("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
        .unwrap_or(0);
    drop(import_db);

    let tmp_str = tmp_path.to_string_lossy();

    // Store the import path for potential restore
    let settings = SettingsRepo::new(state.db);
    settings.set("last_imported_db", &tmp_str).ok();

    Json(json!({
        "status": "imported",
        "temp_path": tmp_str,
        "tracks": track_count,
        "albums": album_count,
        "artists": artist_count,
        "message": "Database file received. Use /system/backups to restore or merge manually.",
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(super) struct DbConnectionTest {
    engine: String,
    connection_string: Option<String>,
}

pub(super) async fn test_db_connection(Json(body): Json<DbConnectionTest>) -> impl IntoResponse {
    match body.engine.as_str() {
        "sqlite" => Json(json!({"status": "ok", "engine": "sqlite"})).into_response(),
        "postgresql" => {
            let conn_str = body
                .connection_string
                .as_deref()
                .unwrap_or("postgresql://localhost/tune");
            if conn_str.starts_with("postgresql://") || conn_str.starts_with("postgres://") {
                Json(json!({
                    "status": "ok",
                    "engine": "postgresql",
                    "message": "PostgreSQL support planned for v2.1. Connection string format is valid.",
                }))
                .into_response()
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid connection string, must start with postgresql:// or postgres://"})),
                )
                    .into_response()
            }
        }
        other => (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": format!("unknown engine: {other}. Supported: sqlite, postgresql")}),
            ),
        )
            .into_response(),
    }
}

pub(super) async fn migrate_database(State(state): State<AppState>) -> impl IntoResponse {
    let tracks = TrackRepo::new(state.db.clone()).count().unwrap_or(0);
    let albums = AlbumRepo::new(state.db.clone()).count().unwrap_or(0);
    let artists = ArtistRepo::new(state.db).count().unwrap_or(0);

    Json(json!({
        "status": "not_implemented",
        "message": "SQLite -> PostgreSQL migration planned for v2.1. Current engine: SQLite.",
        "current_engine": "sqlite",
        "row_counts": {
            "artists": artists,
            "albums": albums,
            "tracks": tracks,
        },
    }))
}
