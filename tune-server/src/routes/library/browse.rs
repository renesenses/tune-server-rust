use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct BrowseQuery {
    path: String,
}

#[derive(Deserialize)]
pub(super) struct FolderQuery {
    path: Option<String>,
}

pub(super) async fn browse_roots(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    let roots: Vec<Value> = dirs
        .iter()
        .map(|d| {
            let norm = tune_core::scanner::walker::normalize_path(d);
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tracks WHERE file_path LIKE ?",
                    rusqlite::params![format!("{norm}%")],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let name = std::path::Path::new(&norm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&norm);
            json!({ "path": norm, "name": name, "track_count": count })
        })
        .collect();
    drop(conn);
    Ok(Json(json!({ "roots": roots })))
}

pub(super) async fn browse_directory(
    State(state): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> Result<impl IntoResponse, AppError> {
    let normalized_query = tune_core::scanner::walker::normalize_path(&q.path);
    let resolved = std::path::Path::new(&normalized_query);
    if !resolved.is_absolute() || !resolved.exists() {
        return Err(AppError::bad_request("invalid path"));
    }

    // Verify path is under a configured music dir.
    // Use std::path::Path::starts_with for OS-aware prefix matching
    // (handles both `/` and `\` separators on Windows).
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let music_root = dirs.iter().find(|d| {
        let norm_dir = tune_core::scanner::walker::normalize_path(d);
        resolved.starts_with(&norm_dir)
    });
    let Some(music_root) = music_root else {
        return Err(AppError::bad_request(
            "path not under a configured music directory",
        ));
    };
    let music_root = tune_core::scanner::walker::normalize_path(music_root);

    // List subdirectories
    let mut subdirs: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&q.path) {
        let conn = state
            .db
            .connection()
            .lock()
            .map_err(|e| AppError::internal(format!("{e}")))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_path = path.to_string_lossy().to_string();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.starts_with('.') {
                    continue;
                }
                let track_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE ?",
                        rusqlite::params![format!("{dir_path}%")],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                subdirs.push(json!({ "name": name, "path": dir_path, "track_count": track_count }));
            }
        }
        drop(conn);
    }
    subdirs.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });

    // List tracks in this directory (not recursive — only direct children)
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    // Use a LIKE pattern that matches the directory prefix and filter in app
    // for direct children only.
    let dir_prefix = format!("{}%", q.path);
    let tracks: Vec<Value> = conn
        .prepare("SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, t.disc_number, t.track_number, t.duration_ms, t.file_path, t.format, t.sample_rate, t.bit_depth, t.genre, t.year, al.cover_path FROM tracks t LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id WHERE t.file_path LIKE ? ORDER BY t.disc_number, t.track_number, t.title")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![dir_prefix], |row| {
                let file_path: Option<String> = row.get(9).ok();
                let is_direct = file_path.as_ref().map(|fp| {
                    let parent = std::path::Path::new(fp).parent().and_then(|p| p.to_str()).unwrap_or("");
                    parent == q.path
                }).unwrap_or(false);
                Ok((is_direct, json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "album_id": row.get::<_, Option<i64>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "artist_id": row.get::<_, Option<i64>>(4).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(5).ok().flatten(),
                    "disc_number": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "track_number": row.get::<_, Option<i32>>(7).ok().flatten(),
                    "duration_ms": row.get::<_, Option<i64>>(8).ok().flatten(),
                    "file_path": file_path,
                    "format": row.get::<_, Option<String>>(10).ok().flatten(),
                    "sample_rate": row.get::<_, Option<i32>>(11).ok().flatten(),
                    "bit_depth": row.get::<_, Option<i32>>(12).ok().flatten(),
                    "genre": row.get::<_, Option<String>>(13).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(14).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(15).ok().flatten(),
                })))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map(|v| v.into_iter().filter(|(direct, _)| *direct).map(|(_, v)| v).collect())
        })
        .unwrap_or_default();
    drop(conn);

    // Parent path
    let parent = if q.path != music_root {
        resolved.parent().map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    Ok(Json(json!({
        "path": q.path,
        "parent": parent,
        "music_root": music_root,
        "directories": subdirs,
        "tracks": tracks,
    })))
}

pub(super) async fn browse_folders(
    State(state): State<AppState>,
    Query(q): Query<FolderQuery>,
) -> axum::response::Response {
    // /library/folders?path=... is an alias for browse_directory
    // Without a path param, return browse roots
    match q.path {
        Some(ref p) if !p.is_empty() => {
            browse_directory(State(state), Query(BrowseQuery { path: p.clone() }))
                .await
                .into_response()
        }
        _ => {
            let roots_json = browse_roots(State(state)).await;
            roots_json.into_response()
        }
    }
}
