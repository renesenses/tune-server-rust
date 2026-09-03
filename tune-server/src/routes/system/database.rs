use std::time::Instant;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

pub(super) async fn database_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // #3182 : le commentaire d'origine — « the migration version is a SQLite
    // notion; PG tracks its own and reports 0 » — décrivait le défaut au lieu
    // de le corriger. PostgreSQL tient bien SA table `schema_version`, elle est
    // lisible, et « 0 » face à un `latest` SQLite rendait `up_to_date: false`
    // à demeure sur une base parfaitement à jour.
    let engine = state.backend.engine();
    let version = super::version_de_schema(&state);
    let latest = super::version_de_schema_cible(engine);
    // `null` quand l'un des deux manque : une comparaison impossible ne rend
    // pas `false`, elle ne rend rien.
    let up_to_date = match (version, latest) {
        (Some(v), Some(l)) => Some(v >= l),
        _ => None,
    };
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

    Ok(Json(json!({
        "engine": engine.as_str(),
        "migration_version": version,
        "latest_version": latest,
        "up_to_date": up_to_date,
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
    // The contentless FTS index is a SQLite feature; PostgreSQL has no
    // equivalent to rebuild.
    let db = match state.sqlite() {
        Ok(db) => db,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    let conn = match db.connection().lock() {
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
    if let Ok(db) = state.sqlite() {
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
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

/// Plafond de corps propre à l'import de base.
///
/// La limite globale du serveur est de 50 Mo (`routes/mod.rs`), soit cinq fois
/// moins que ce que pèse l'export d'une bibliothèque ordinaire : **256 Mo pour
/// 47 000 pistes**, mesuré le 30/08/2026. Au-dessus, la requête était coupée
/// avant d'atteindre ce handler et le testeur recevait « no file provided »
/// alors qu'il avait bien fourni un fichier (#2849, Johannes Henke, Synology).
/// Comme `support.rs`, la route pose donc sa propre limite, au-dessus de la
/// globale. 2 Gio et pas davantage : la valeur tient dans un `usize` 32 bits,
/// que la cible armv7 utilise encore.
pub(super) const IMPORT_DB_BODY_LIMIT: usize = 2 * 1024 * 1024 * 1024;

/// Rend l'erreur multipart telle qu'elle est, au lieu de la déguiser.
///
/// L'ancienne boucle `while let Ok(Some(field))` sortait silencieusement sur
/// une `Err`, et `field.bytes().await.ok()` en faisait un `None` : **toute**
/// panne de transport — dépassement de taille compris — ressortait en
/// `400 no file provided`, le message le plus trompeur possible pour qui vient
/// justement de fournir un fichier. Un dépassement sort maintenant en 413.
fn multipart_failure(err: axum::extract::multipart::MultipartError) -> Response {
    let status = err.status();
    let detail = err.body_text();
    tracing::warn!(status = %status, error = %detail, "database_import_multipart_error");
    let hint = (status == StatusCode::PAYLOAD_TOO_LARGE).then(|| {
        format!(
            "the uploaded database exceeds this route's limit of {} MB",
            IMPORT_DB_BODY_LIMIT / (1024 * 1024)
        )
    });
    (status, Json(json!({"error": detail, "hint": hint}))).into_response()
}

/// Remplace la base active par un fichier SQLite téléversé.
///
/// Avant le 30/08/2026 ce handler écrivait le fichier reçu dans `/tmp`, comptait
/// trois tables et s'arrêtait là : la base active n'était jamais remplacée,
/// aucune sauvegarde n'était prise, et le chemin mémorisé (`last_imported_db`)
/// n'était relu nulle part. L'écran annonçait pourtant « Import successful ».
/// Les trois promesses du dialogue de confirmation — remplacer la base, prendre
/// une sauvegarde de sécurité, redémarrer ensuite — sont désormais tenues.
pub(super) async fn database_import(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse {
    // Le fichier reçu est une base SQLite : en PostgreSQL le serveur n'ouvre
    // même plus `db_path`, et l'écraser donnerait à l'opérateur l'illusion
    // d'une restauration. Même refus explicite que les sauvegardes fichier.
    if state.db.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "database import is SQLite-only; this server runs on PostgreSQL — \
                          use pg_restore against TUNE_DATABASE_URL",
            })),
        )
            .into_response();
    }
    let db_path = state.config.db_path.clone();
    if db_path == ":memory:" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "cannot import into an in-memory database"})),
        )
            .into_response();
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let name = field.name().unwrap_or("").to_string();
                if name == "file" || name == "database" {
                    match field.bytes().await {
                        Ok(b) => file_bytes = Some(b.to_vec()),
                        Err(e) => return multipart_failure(e),
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return multipart_failure(e),
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

    // Contrôles avant de toucher à la base en service. Un refus ici laisse
    // l'installation intacte ; un fichier tronqué appliqué, non.
    let counts = match inspect_import_candidate(&tmp_path) {
        Ok(c) => c,
        Err(refusal) => {
            let _ = std::fs::remove_file(&tmp_path);
            return (StatusCode::BAD_REQUEST, Json(json!({"error": refusal}))).into_response();
        }
    };

    // La sauvegarde de sécurité que le dialogue de confirmation promet.
    let safety = tune_core::db_backup::create_backup(&db_path);
    if safety.is_none() {
        tracing::warn!("database_import_safety_backup_failed");
    }

    // Vider le WAL de la base sortante : sans cela SQLite le rejoue par-dessus
    // le fichier fraîchement copié et rend un mélange des deux bases.
    if let Ok(db) = state.sqlite() {
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    }

    match tune_core::db_backup::replace_database(&db_path, &tmp_path) {
        Ok(size) => {
            let _ = std::fs::remove_file(&tmp_path);
            tracing::info!(
                size,
                tracks = counts.tracks,
                backup = safety.as_ref().map(|b| b.filename.as_str()).unwrap_or("-"),
                "database_imported"
            );
            Json(json!({
                "imported": true,
                "engine": "sqlite",
                "size": size,
                "restart_required": true,
                "backup": safety.map(|b| b.filename),
                "tracks": counts.tracks,
                "albums": counts.albums,
                "artists": counts.artists,
            }))
            .into_response()
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            tracing::warn!(error = %e, "database_import_replace_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("could not replace the active database: {e}")})),
            )
                .into_response()
        }
    }
}

/// Ce qu'un candidat à l'import contient, une fois jugé applicable.
#[derive(Debug)]
struct ImportCounts {
    tracks: i64,
    albums: i64,
    artists: i64,
}

/// Juge un fichier reçu AVANT de remplacer quoi que ce soit.
///
/// Rend le motif de refus, en clair, quand le fichier n'est pas une base Tune :
/// un fichier tronqué s'ouvre sans broncher (SQLite ne lit l'en-tête qu'à la
/// première requête), et une base d'un autre logiciel s'ouvre parfaitement.
fn inspect_import_candidate(path: &std::path::Path) -> Result<ImportCounts, String> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("not a valid SQLite file: {e}"))?;

    let integrity: String = db
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("not a valid SQLite file: {e}"))?;
    if integrity != "ok" {
        return Err(format!("the uploaded database is corrupt: {integrity}"));
    }

    for table in ["tracks", "albums", "artists"] {
        let present: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if present == 0 {
            return Err(format!(
                "this file is a SQLite database but not a Tune one: table `{table}` is missing"
            ));
        }
    }

    let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    Ok(ImportCounts {
        tracks: count("SELECT COUNT(*) FROM tracks"),
        albums: count("SELECT COUNT(*) FROM albums"),
        artists: count("SELECT COUNT(*) FROM artists"),
    })
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
///
/// Même portée que [`persist_database_url`] juste au-dessus : la migration
/// SQLite → PostgreSQL n'existe que sous `postgres`, et son unique appelant
/// vit dans le bloc `#[cfg(feature = "postgres")]` de `migrate_database`.
/// L'attribut manquait ici seul — d'où une fonction sans appelant dans toutes
/// les configurations de la CI, qui ne compile pas `postgres`.
#[cfg(feature = "postgres")]
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
        // `state.db` est devenu Option<SqliteDb> avec la suppression du
        // split-brain : il n'y a pas de store SQLite quand le serveur tourne
        // deja sur PostgreSQL. Migrer SQLite -> PG n'a alors aucun sens, et
        // c'est un refus explicite plutot qu'un message obscur.
        // Passe par l'accesseur introduit avec la suppression du split-brain,
        // comme les deux autres usages de ce fichier (l.93 et l.144) : le
        // message d'indisponibilité vit à un seul endroit. Formulation de JP
        // dans sa PR #1424, que ce correctif rejoint.
        let sqlite = match state.sqlite() {
            Ok(db) => db,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "status": "error",
                        "error": "this server already runs on PostgreSQL — there is no SQLite \
                                  database to migrate from",
                    })),
                )
                    .into_response();
            }
        };

        let start = Instant::now();
        match tune_core::db::pg_migrate::migrate_sqlite_to_pg(sqlite, pg_url).await {
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

#[cfg(test)]
mod tests_import {
    use super::*;

    /// Le plafond de cette route doit dépasser la limite globale, sinon il ne
    /// sert à rien : la requête est coupée avant d'arriver au handler.
    ///
    /// Le repère de 256 Mo n'est pas théorique — c'est l'export mesuré le
    /// 30/08/2026 d'une bibliothèque de 47 000 pistes.
    #[test]
    fn plafond_import_depasse_la_limite_globale_et_un_export_reel() {
        const LIMITE_GLOBALE: usize = 50 * 1024 * 1024;
        const EXPORT_MESURE: usize = 256 * 1024 * 1024;
        assert!(IMPORT_DB_BODY_LIMIT > LIMITE_GLOBALE);
        assert!(IMPORT_DB_BODY_LIMIT > EXPORT_MESURE);
    }

    fn base_tune_minimale(chemin: &std::path::Path) {
        let db = rusqlite::Connection::open(chemin).unwrap();
        db.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY);
             CREATE TABLE albums (id INTEGER PRIMARY KEY);
             CREATE TABLE tracks (id INTEGER PRIMARY KEY);
             INSERT INTO tracks (id) VALUES (1), (2);
             INSERT INTO albums (id) VALUES (1);",
        )
        .unwrap();
    }

    #[test]
    fn une_base_tune_est_acceptee_et_comptee() {
        let dir = tempfile::tempdir().unwrap();
        let chemin = dir.path().join("candidate.db");
        base_tune_minimale(&chemin);

        let counts = inspect_import_candidate(&chemin).expect("une base Tune doit passer");
        assert_eq!(counts.tracks, 2);
        assert_eq!(counts.albums, 1);
        assert_eq!(counts.artists, 0);
    }

    /// Contre-épreuve du test précédent : ce qui n'est pas une base Tune doit
    /// être REFUSÉ, et refusé avec le motif exact. Sans ces deux cas, le test
    /// d'acceptation ne prouverait rien — une fonction qui dit toujours oui le
    /// passerait aussi.
    #[test]
    fn un_fichier_qui_n_est_pas_une_base_tune_est_refuse() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Pas du SQLite du tout : le refus tombe à la lecture de l'en-tête.
        let texte = dir.path().join("notes.txt");
        std::fs::write(&texte, b"ceci n'est pas une base de donnees").unwrap();
        let motif = inspect_import_candidate(&texte).unwrap_err();
        assert!(
            motif.contains("not a valid SQLite file") || motif.contains("corrupt"),
            "motif inattendu: {motif}"
        );

        // 2. Du SQLite valide, mais d'un autre logiciel : les tables manquent.
        let etrangere = dir.path().join("autre.db");
        let db = rusqlite::Connection::open(&etrangere).unwrap();
        db.execute_batch("CREATE TABLE recettes (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(db);
        let motif = inspect_import_candidate(&etrangere).unwrap_err();
        assert!(motif.contains("not a Tune one"), "motif inattendu: {motif}");
        assert!(motif.contains("tracks"), "le motif doit nommer la table");
    }
}
