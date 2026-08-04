use std::time::Instant;

use axum::Json;
use axum::extract::{Query, State};
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
    // migration check uses SqliteDb directly (sqlite-specific)
    let version = migrations::current_version(&state.db).unwrap_or(0);
    let latest = migrations::latest_version();
    let row = state.backend.query_one(
        "SELECT \
         (SELECT COUNT(*) FROM artists WHERE id IN (SELECT DISTINCT artist_id FROM albums WHERE artist_id IS NOT NULL)), \
         (SELECT COUNT(*) FROM albums), \
         (SELECT COUNT(*) FROM tracks)",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let (artists, albums, tracks) = row
        .map(|r| {
            (
                r.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                r.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));

    let engine_name = format!("{:?}", state.backend.engine()).to_lowercase();
    Ok(Json(json!({
        "engine": engine_name,
        "migration_version": version,
        "latest_version": latest,
        "up_to_date": version >= latest,
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
    })))
}

pub(super) async fn database_optimize(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let start = Instant::now();
    let sql = if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        "PRAGMA optimize; VACUUM; ANALYZE;"
    } else {
        "ANALYZE;"
    };
    match state.backend.execute_batch(sql) {
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
pub(super) async fn rebuild_fts(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> impl IntoResponse {
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
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    // Use the resolved path from config: on Windows/macOS a relative db_path is
    // rewritten to the per-user data dir at startup, so re-reading the raw env
    // var here would point at a file that does not exist (os error 2).
    let db_path = state.config.db_path.clone();
    if db_path == ":memory:" {
        return Ok((StatusCode::BAD_REQUEST, "cannot export in-memory database").into_response());
    }

    // SQLite-specific WAL checkpoint before exporting the DB file
    if state.backend.engine() == tune_core::db::engine::Engine::Sqlite {
        state
            .db
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .ok();
    }

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

/// Turn a raw PostgreSQL connection error into a user-facing message. When the
/// target database doesn't exist yet (the migrate/test flow creates the schema
/// but NOT the database itself), tell the user to create it first instead of
/// surfacing the raw driver error (JP: `database "tune" does not exist`).
/// Returns (message, optional hint).
#[cfg(feature = "postgres")]
fn enrich_pg_error(err: &str, conn_str: &str) -> (String, Option<String>) {
    let lower = err.to_lowercase();
    if lower.contains("does not exist") && lower.contains("database") {
        // Best-effort DB name from the DSN path (…/tune, minus any ?query).
        let db = conn_str
            .rsplit('/')
            .next()
            .and_then(|s| s.split('?').next())
            .filter(|s| !s.is_empty())
            .unwrap_or("tune");
        let hint = format!(
            "La base de données « {db} » n'existe pas encore. Créez-la d'abord \
             (par ex. : CREATE DATABASE {db};), puis relancez."
        );
        (format!("{err} — {hint}"), Some(hint))
    } else {
        (err.to_string(), None)
    }
}

#[derive(Deserialize, Default)]
pub(super) struct DbConnectionTest {
    /// Engine type: "sqlite" or "postgresql". Defaults to "postgresql".
    engine: Option<String>,
    /// Connection string (for postgresql)
    connection_string: Option<String>,
    /// Alternative field name: URL
    url: Option<String>,
}

/// Query-string form of the same params. The web client posts the DSN as
/// `?url=<encoded>&target=<engine>` with NO JSON body, so reading only a JSON
/// body made axum's `Json` extractor reject the bodyless request with a non-JSON
/// error — which the client then hit with `.json()`, surfacing the cryptic
/// "JSON.parse: unexpected character at line 1 column 1" (JP, PG migration).
/// Accept both: query first, JSON body as fallback for API callers.
#[derive(Deserialize, Default)]
pub(super) struct DbConnQuery {
    engine: Option<String>,
    target: Option<String>,
    connection_string: Option<String>,
    url: Option<String>,
}

pub(super) async fn test_db_connection(
    Query(q): Query<DbConnQuery>,
    body: Option<Json<DbConnectionTest>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let engine = q
        .engine
        .or(q.target)
        .or(body.engine)
        .unwrap_or_else(|| "postgresql".to_string());
    let engine = engine.as_str();
    let conn_owned = q
        .url
        .or(q.connection_string)
        .or(body.url)
        .or(body.connection_string)
        .unwrap_or_else(|| "postgresql://localhost/tune".to_string());
    let conn_str = conn_owned.as_str();

    match engine {
        "sqlite" => Json(json!({"ok": true, "status": "ok", "engine": "sqlite"})).into_response(),
        "postgresql" | "postgres" => {
            if !conn_str.starts_with("postgresql://") && !conn_str.starts_with("postgres://") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "status": "error", "error": "invalid connection string, must start with postgresql:// or postgres://"})),
                )
                    .into_response();
            }

            #[cfg(feature = "postgres")]
            {
                // Auto-create the target database when it does not exist:
                // nobody should have to open psql for CREATE DATABASE (JP).
                // One retry after a successful create; every other error
                // falls through to the enriched message below.
                let mut created = false;
                let mut attempt = tune_core::db::pg_migrate::test_connection(conn_str).await;
                if let Err(ref e) = attempt {
                    let lower = e.to_lowercase();
                    if lower.contains("does not exist") && lower.contains("database") {
                        match tune_core::db::pg_migrate::ensure_database(conn_str).await {
                            Ok(did) => {
                                created = did;
                                tracing::info!(created, "pg_database_auto_created");
                                attempt =
                                    tune_core::db::pg_migrate::test_connection(conn_str).await;
                            }
                            Err(ce) => {
                                tracing::warn!(error = %ce, "pg_database_auto_create_failed");
                            }
                        }
                    }
                }
                match attempt {
                    Ok(result) => {
                        // Extract short version (e.g. "16.2") from full version string
                        let short_version = result
                            .version
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("unknown");
                        Json(json!({
                            "ok": true,
                            "status": "ok",
                            "engine": "postgres",
                            "version": short_version,
                            "version_full": result.version,
                            "database_created": created,
                        }))
                        .into_response()
                    }
                    Err(e) => {
                        let (msg, hint) = enrich_pg_error(&e, conn_str);
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "ok": false,
                                "status": "error",
                                "engine": "postgres",
                                "error": msg,
                                "hint": hint,
                            })),
                        )
                            .into_response()
                    }
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

#[derive(Deserialize, Default)]
pub(super) struct MigrateRequest {
    /// PostgreSQL connection URL
    url: Option<String>,
    /// Alternative field name
    connection_string: Option<String>,
}

/// Query-string form: the web client posts `?target=postgres&url=<encoded>` with
/// no JSON body. Accept it (query first, body fallback) so the bodyless request
/// isn't rejected by the `Json` extractor (JP, PG migration).
#[derive(Deserialize, Default)]
pub(super) struct MigrateQuery {
    url: Option<String>,
    connection_string: Option<String>,
    target: Option<String>,
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
/// Persist the PostgreSQL URL into the .env the server reads at startup, so
/// the engine switch survives the restart. Search order mirrors main.rs:
/// CWD/.env first, then (Windows) %LOCALAPPDATA%\TuneServer\.env — created
/// there when none exists.
#[cfg(feature = "postgres")]
fn persist_database_url(url: &str) -> Result<std::path::PathBuf, String> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(".env"));
    }
    #[cfg(windows)]
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(la).join("TuneServer").join(".env"));
    }
    let target = candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .or_else(|| candidates.last().cloned())
        .ok_or_else(|| "no .env location available".to_string())?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&target)
        .map(|c| c.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let entry = format!("TUNE_DATABASE_URL={url}");
    if let Some(l) = lines
        .iter_mut()
        .find(|l| l.trim_start().starts_with("TUNE_DATABASE_URL="))
    {
        *l = entry;
    } else {
        lines.push(entry);
    }
    std::fs::write(&target, lines.join("\n") + "\n").map_err(|e| format!("write .env: {e}"))?;
    Ok(target)
}

/// Restart the server after a successful migration so it comes back on
/// PostgreSQL. Unlike the update flow there is no binary swap involved, so
/// spawning the SAME executable is safe on Windows too (the update path must
/// NOT spawn — see update.rs — but here no .bat is waiting on our exit).
fn restart_after_migration() {
    tokio::spawn(async {
        // Let the HTTP response flush to the client first.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let exe = std::env::current_exe().unwrap_or_default();
        let args: Vec<String> = std::env::args().skip(1).collect();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            tracing::info!(exe = %exe.display(), "migrate_reexec");
            let err = std::process::Command::new(&exe).args(&args).exec();
            tracing::warn!(error = %err, "migrate_reexec_failed — falling back to spawn+exit");
        }
        match std::process::Command::new(&exe)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
        {
            Ok(child) => {
                tracing::info!(pid = child.id(), "migrate_new_process_spawned");
            }
            Err(e) => {
                tracing::warn!(error = %e, "migrate_restart_spawn_failed — manual restart required");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::process::exit(0);
    });
}

pub(super) async fn migrate_database(
    State(state): State<AppState>,
    Query(q): Query<MigrateQuery>,
    body: Option<Json<MigrateRequest>>,
) -> impl IntoResponse {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let pg_url_owned = q
        .url
        .or(q.connection_string)
        .or(body.url)
        .or(body.connection_string)
        .unwrap_or_default();
    // target is accepted (client sends ?target=postgres) but this handler only
    // implements SQLite → PostgreSQL; the DSN presence drives it.
    let _ = q.target;
    let pg_url = pg_url_owned.as_str();

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
        // Same auto-create as test_db_connection: a migrate against a
        // never-created database should just work (JP).
        if let Err(e) = tune_core::db::pg_migrate::ensure_database(pg_url).await {
            tracing::warn!(error = %e, "pg_database_auto_create_failed_pre_migrate");
        }
        let start = Instant::now();
        match tune_core::db::pg_migrate::migrate_sqlite_to_pg(&state.db, pg_url).await {
            Ok(result) => {
                let duration_ms = start.elapsed().as_millis() as u64;
                // The UI promises « le serveur va redémarrer » — deliver it:
                // persist the engine switch, then re-exec on PostgreSQL (JF:
                // no restart happened and a manual relaunch stayed on SQLite).
                let env_path = match persist_database_url(pg_url) {
                    Ok(p) => {
                        tracing::info!(path = %p.display(), "migrate_database_url_persisted");
                        restart_after_migration();
                        Some(p.display().to_string())
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "migrate_database_url_persist_failed — NOT restarting");
                        None
                    }
                };
                Json(json!({
                    "status": "complete",
                    "restarting": env_path.is_some(),
                    "env_path": env_path,
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
            Err(e) => {
                let (msg, hint) = enrich_pg_error(&e, pg_url);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "status": "error",
                        "error": msg,
                        "hint": hint,
                        "source": {
                            "engine": "sqlite",
                            "artists": artists,
                            "albums": albums,
                            "tracks": tracks,
                        },
                    })),
                )
                    .into_response()
            }
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
