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

/// POST /system/database/rebuild-fts — Rebuild all FTS5 search indexes.
///
/// Necessary after manual DB corrections (sqlite3 CLI, DB Browser, etc.)
/// because the FTS triggers only fire for writes that go through SQLite's
/// trigger mechanism. Direct INSERT/UPDATE/DELETE via external tools can
/// leave the FTS indexes out of sync, causing search to return stale or
/// empty results while stats and browse still show the correct counts.
///
/// Also performs a WAL checkpoint so that read-only connections (used by
/// the browse/list endpoints) immediately see any recent writes.
pub(super) async fn rebuild_fts(State(state): State<AppState>) -> impl IntoResponse {
    let conn = match state.db.connection().lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db lock: {e}")})),
            )
                .into_response();
        }
    };

    let result = tune_core::library::full_text_search::rebuild_fts_contentless(&conn);

    // Checkpoint WAL so read-only connections see the rebuilt FTS data immediately
    conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").ok();
    drop(conn);

    match result {
        Ok(rows) => Json(json!({
            "status": "ok",
            "rows_indexed": rows,
            "message": "FTS indexes rebuilt successfully. Search should now reflect current library state.",
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e})),
        )
            .into_response(),
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    /// Engine type: "sqlite" or "postgresql". Defaults to "postgresql".
    engine: Option<String>,
    /// Connection string (for postgresql)
    connection_string: Option<String>,
    /// Alternative field name: URL
    url: Option<String>,
}

pub(super) async fn test_db_connection(Json(body): Json<DbConnectionTest>) -> impl IntoResponse {
    let engine = body.engine.as_deref().unwrap_or("postgresql");
    let conn_str = body
        .url
        .as_deref()
        .or(body.connection_string.as_deref())
        .unwrap_or("postgresql://localhost/tune");

    match engine {
        "sqlite" => Json(json!({"status": "ok", "engine": "sqlite"})).into_response(),
        "postgresql" | "postgres" => {
            if !conn_str.starts_with("postgresql://") && !conn_str.starts_with("postgres://") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid connection string, must start with postgresql:// or postgres://"})),
                )
                    .into_response();
            }

            #[cfg(feature = "postgres")]
            {
                match tune_core::db::pg_migrate::test_connection(conn_str).await {
                    Ok(result) => {
                        // Extract short version (e.g. "16.2") from full version string
                        let short_version = result
                            .version
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("unknown");
                        Json(json!({
                            "status": "ok",
                            "engine": "postgres",
                            "version": short_version,
                            "version_full": result.version,
                        }))
                        .into_response()
                    }
                    Err(e) => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "status": "error",
                            "engine": "postgres",
                            "error": e,
                        })),
                    )
                        .into_response(),
                }
            }

            #[cfg(not(feature = "postgres"))]
            {
                let _ = conn_str;
                (
                    StatusCode::NOT_IMPLEMENTED,
                    Json(json!({
                        "status": "error",
                        "engine": "postgres",
                        "error": "PostgreSQL support not compiled. Rebuild with --features postgres.",
                    })),
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

#[derive(Deserialize)]
pub(super) struct MigrateRequest {
    /// PostgreSQL connection URL
    url: Option<String>,
    /// Alternative field name
    connection_string: Option<String>,
}

/// POST /system/database/migrate
///
/// One-shot migration: copies all data from the current SQLite database
/// to a PostgreSQL instance. The PG schema is created automatically.
/// Idempotent — safe to run multiple times (ON CONFLICT DO NOTHING).
///
/// Request body: `{"url": "postgresql://user:pass@host:5432/dbname"}`
///
/// This does NOT switch the running engine — Tune continues to use
/// SQLite after the migration. The PG database is populated and ready
/// for a future engine switch.
pub(super) async fn migrate_database(
    State(state): State<AppState>,
    Json(body): Json<MigrateRequest>,
) -> impl IntoResponse {
    let pg_url = body
        .url
        .as_deref()
        .or(body.connection_string.as_deref())
        .unwrap_or("");

    if pg_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "error": "missing 'url' field. Provide a PostgreSQL connection URL.",
                "example": {"url": "postgresql://tune:tune2026@localhost:5432/tune"},
            })),
        )
            .into_response();
    }

    if !pg_url.starts_with("postgresql://") && !pg_url.starts_with("postgres://") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "error": "invalid URL, must start with postgresql:// or postgres://",
            })),
        )
            .into_response();
    }

    // Pre-flight: count rows to report in the response
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);

    #[cfg(feature = "postgres")]
    {
        let start = Instant::now();
        match tune_core::db::pg_migrate::migrate_sqlite_to_pg(&state.db, pg_url).await {
            Ok(result) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                Json(json!({
                    "status": "complete",
                    "tables_migrated": result.tables_migrated,
                    "total_rows": result.total_rows,
                    "duration_ms": duration_ms,
                    "source": {
                        "engine": "sqlite",
                        "artists": artists,
                        "albums": albums,
                        "tracks": tracks,
                    },
                    "details": result.details,
                    "errors": result.errors,
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "error": e,
                    "source": {
                        "engine": "sqlite",
                        "artists": artists,
                        "albums": albums,
                        "tracks": tracks,
                    },
                })),
            )
                .into_response(),
        }
    }

    #[cfg(not(feature = "postgres"))]
    {
        let _ = (pg_url, tracks, albums, artists);
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "status": "error",
                "error": "PostgreSQL support not compiled. Rebuild with --features postgres.",
            })),
        )
            .into_response()
    }
}
