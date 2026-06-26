use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

#[derive(Deserialize)]
pub(super) struct GenreQuery {
    query: Option<String>,
}

pub(super) async fn genre_tree(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    // Collect all individual genres from both the `genres` JSON array
    // and the legacy `genre` text column (splitting multi-genre strings).
    let raw_genres: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT genre, genres FROM tracks WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '') GROUP BY genre, genres")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).unwrap_or(None),
                    row.get::<_, Option<String>>(1).unwrap_or(None),
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    let mut genre_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (genre_col, genres_col) in &raw_genres {
        // Prefer the structured genres JSON array if present
        if let Some(json_str) = genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                for g in arr {
                    let trimmed = g.trim().to_string();
                    if !trimmed.is_empty() {
                        genre_set.insert(trimmed);
                    }
                }
                continue;
            }
        }
        // Fall back to splitting the legacy genre column
        if let Some(raw) = genre_col {
            for g in tune_core::metadata::split_genre_tag(raw) {
                if !g.is_empty() {
                    genre_set.insert(g);
                }
            }
        }
    }

    let genres: Vec<String> = genre_set.into_iter().collect();

    // Load saved tree from settings (persisted by PUT /genre-tree).
    // If a saved tree exists, use it as the base and add any new genres
    // found in the library that aren't already in any branch.
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut tree: std::collections::BTreeMap<String, Vec<String>> = settings
        .get("genre_tree")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if tree.is_empty() {
        for genre in &genres {
            tree.entry(genre.clone()).or_default();
        }
    } else {
        let mut classified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (parent, children) in &tree {
            classified.insert(parent.clone());
            for child in children {
                classified.insert(child.clone());
            }
        }
        for genre in &genres {
            if !classified.contains(genre) {
                tree.entry(genre.clone()).or_default();
            }
        }
    }

    Ok(Json(json!({
        "tree": tree,
        "genres": genres,
        "total": genres.len(),
    })))
}

pub(super) async fn update_genre_tree(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let tree_val = body.get("tree").unwrap_or(&body);
    settings.set("genre_tree", &tree_val.to_string()).ok();
    Json(json!({"updated": true}))
}

pub(super) async fn list_genres(
    State(state): State<AppState>,
    Query(params): Query<GenreQuery>,
) -> Result<Json<Value>, AppError> {
    let conn = state
        .db
        .connection()
        .lock()
        .map_err(|e| AppError::internal(format!("{e}")))?;
    // Collect genre + genres columns from all albums
    let raw: Vec<(Option<String>, Option<String>)> = conn
        .prepare("SELECT genre, genres FROM albums WHERE (genre IS NOT NULL AND genre != '') OR (genres IS NOT NULL AND genres != '')")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).unwrap_or(None),
                    row.get::<_, Option<String>>(1).unwrap_or(None),
                ))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);

    // Split multi-genre values and count individual genres
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for (genre_col, genres_col) in &raw {
        let mut genres_for_album: Vec<String> = Vec::new();
        // Prefer the structured genres JSON array if present
        if let Some(json_str) = genres_col {
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(json_str) {
                genres_for_album = arr
                    .into_iter()
                    .map(|g| g.trim().to_string())
                    .filter(|g| !g.is_empty())
                    .collect();
            }
        }
        // Fall back to splitting the legacy genre column
        if genres_for_album.is_empty() {
            if let Some(raw_genre) = genre_col {
                genres_for_album = tune_core::metadata::split_genre_tag(raw_genre);
            }
        }
        for g in genres_for_album {
            *counts.entry(g).or_insert(0) += 1;
        }
    }

    // Filter by query parameter (case-insensitive LIKE match)
    let filter = params.query.map(|q| q.to_lowercase());

    let items: Vec<Value> = counts
        .iter()
        .filter(|(name, _)| match &filter {
            Some(q) => name.to_lowercase().contains(q),
            None => true,
        })
        .map(|(name, count)| json!({ "name": name, "count": count }))
        .collect();

    Ok(Json(json!(items)))
}

pub(super) async fn genre_albums(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Json<Value> {
    let decoded = urlencoding::decode(&name).unwrap_or_else(|_| name.clone().into());
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_by_genre(&decoded).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}
