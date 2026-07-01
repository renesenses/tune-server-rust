use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;
use unicode_normalization::UnicodeNormalization;

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
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let roots: Vec<Value> = dirs
        .iter()
        .map(|d| {
            let norm = tune_core::scanner::walker::normalize_path(d);
            let norm_nfc: String = norm.nfc().collect();
            let sep = std::path::MAIN_SEPARATOR;
            let pattern = format!("{norm_nfc}{sep}%");
            let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
                "$1"
            } else {
                "?1"
            };
            let count: i64 = match state.backend.query_one(
                &format!("SELECT COUNT(*) FROM tracks WHERE file_path LIKE {ph}"),
                &[&pattern as &dyn tune_core::db::backend::ToSqlValue],
            ) {
                Ok(Some(cols)) => cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                Ok(None) => 0,
                Err(e) => {
                    warn!(path = %norm_nfc, error = %e, "browse_root_count_failed");
                    0
                }
            };
            if count == 0 {
                let sample = state
                    .backend
                    .query_one("SELECT file_path FROM tracks LIMIT 1", &[])
                    .ok()
                    .flatten()
                    .and_then(|r| r.first().and_then(|v| v.as_string()));
                warn!(
                    music_dir = %norm_nfc,
                    pattern = %pattern,
                    sample_file_path = ?sample,
                    "browse_root_zero_tracks"
                );
            }
            let name = std::path::Path::new(&norm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&norm);
            json!({ "path": norm, "name": name, "track_count": count })
        })
        .collect();
    Ok(Json(json!({ "roots": roots })))
}

pub(super) async fn browse_directory(
    State(state): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> Result<impl IntoResponse, AppError> {
    let normalized_query: String = tune_core::scanner::walker::normalize_path(&q.path)
        .nfc()
        .collect();
    let resolved = std::path::Path::new(&normalized_query);
    if !resolved.is_absolute() || !resolved.exists() {
        return Err(AppError::bad_request("invalid path"));
    }

    // Verify path is under a configured music dir.
    // Use std::path::Path::starts_with for OS-aware prefix matching
    // (handles both `/` and `\` separators on Windows).
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
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
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dir_path: String = path.to_string_lossy().nfc().collect();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.starts_with('.') {
                    continue;
                }
                let sep = std::path::MAIN_SEPARATOR;
                let pattern = format!("{dir_path}{sep}%");
                let track_count: i64 = match state.backend.query_one(
                    &format!(
                        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE {}",
                        if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
                            "$1"
                        } else {
                            "?1"
                        }
                    ),
                    &[&pattern as &dyn tune_core::db::backend::ToSqlValue],
                ) {
                    Ok(Some(cols)) => cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    Ok(None) => 0,
                    Err(e) => {
                        warn!(path = %dir_path, error = %e, "browse_dir_count_failed");
                        0
                    }
                };
                subdirs.push(json!({ "name": name, "path": dir_path, "track_count": track_count }));
            }
        }
        // conn removed — using state.backend
    }
    subdirs.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });

    // List tracks in this directory (not recursive — only direct children)
    let sep = std::path::MAIN_SEPARATOR;
    let dir_prefix = format!("{}{sep}%", normalized_query);
    let ph = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1"
    } else {
        "?1"
    };
    let sql = format!(
        "SELECT t.id, t.title, t.album_id, al.title, t.artist_id, ar.name, \
               t.disc_number, t.track_number, t.duration_ms, t.file_path, \
               t.format, t.sample_rate, t.bit_depth, t.genre, t.year, al.cover_path \
               FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
               LEFT JOIN artists ar ON t.artist_id = ar.id \
               WHERE t.file_path LIKE {ph} \
               ORDER BY CAST(t.disc_number AS INTEGER), CAST(t.track_number AS INTEGER), t.title"
    );
    let rows = state
        .backend
        .query_many(
            &sql,
            &[&dir_prefix as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .filter_map(|cols| {
            let file_path = cols.get(9).and_then(|v| v.as_string());
            let is_direct = file_path
                .as_ref()
                .map(|fp| {
                    let parent = std::path::Path::new(fp)
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or("");
                    parent == normalized_query
                })
                .unwrap_or(false);
            if !is_direct {
                return None;
            }
            Some(json!({
                "id": cols.first().and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "album_id": cols.get(2).and_then(|v| v.as_i64()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "artist_id": cols.get(4).and_then(|v| v.as_i64()),
                "artist_name": cols.get(5).and_then(|v| v.as_string()),
                "disc_number": cols.get(6).and_then(|v| v.as_i64()),
                "track_number": cols.get(7).and_then(|v| v.as_i64()),
                "duration_ms": cols.get(8).and_then(|v| v.as_i64()),
                "file_path": file_path,
                "format": cols.get(10).and_then(|v| v.as_string()),
                "sample_rate": cols.get(11).and_then(|v| v.as_i64()),
                "bit_depth": cols.get(12).and_then(|v| v.as_i64()),
                "genre": cols.get(13).and_then(|v| v.as_string()),
                "year": cols.get(14).and_then(|v| v.as_i64()),
                "cover_path": cols.get(15).and_then(|v| v.as_string()),
            }))
        })
        .collect();

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
