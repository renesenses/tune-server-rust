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

    // Canonical dedup (case- AND separator-insensitive) so "Classique"/"classique"
    // and "Trip Hop"/"Trip-Hop" collapse into a single genre instead of appearing
    // as duplicate rows (Bilou, #1161). genre_set is a BTreeSet, so the variant
    // that sorts first is the one kept.
    let mut seen_lc: std::collections::HashSet<String> = std::collections::HashSet::new();
    let genres: Vec<String> = genre_set
        .into_iter()
        .filter(|g| seen_lc.insert(tune_core::metadata::genre_key(g)))
        .collect();

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
    }

    let mut classified: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (parent, children) in &tree {
        classified.insert(tune_core::metadata::genre_key(parent));
        for child in children {
            classified.insert(tune_core::metadata::genre_key(child));
        }
    }
    let unclassified: Vec<String> = genres
        .iter()
        .filter(|g| !classified.contains(&tune_core::metadata::genre_key(g)))
        .cloned()
        .collect();

    Ok(Json(json!({
        "tree": tree,
        "genres": genres,
        "unclassified": unclassified,
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

    // Split multi-genre values and count albums per genre. Genres are grouped
    // by a canonical key that ignores case and the space-vs-hyphen separator,
    // so "Trip Hop" and "Trip-Hop" collapse into a single card instead of two
    // (#1161). For each key we keep per-spelling tallies to choose a stable
    // display label.
    let mut groups: std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>> =
        std::collections::BTreeMap::new();
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
        // Dedup within this album by canonical key so an album that tags the
        // same genre under two spellings still counts once toward it.
        let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for g in genres_for_album {
            let key = tune_core::metadata::genre_key(&g);
            if key.is_empty() || !seen_keys.insert(key.clone()) {
                continue;
            }
            *groups.entry(key).or_default().entry(g).or_insert(0) += 1;
        }
    }

    // Filter by query parameter (case-insensitive LIKE match)
    let filter = params.query.map(|q| q.to_lowercase());

    let items: Vec<Value> = groups
        .iter()
        .filter_map(|(_key, variants)| {
            let count: i64 = variants.values().sum();
            // Display label = the most common spelling; ties broken by the
            // lexicographically smallest for a stable, deterministic label.
            let name = variants
                .iter()
                .max_by(|(an, ac), (bn, bc)| ac.cmp(bc).then_with(|| bn.cmp(an)))
                .map(|(name, _)| name.clone())?;
            match &filter {
                Some(q) if !name.to_lowercase().contains(q) => None,
                _ => Some(json!({ "name": name, "count": count })),
            }
        })
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
