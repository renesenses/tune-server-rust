use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use super::SearchQuery;

pub(super) async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(20);
    let artists = ArtistRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    let tracks = TrackRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();

    // --- Extended metadata search (Approach B) ---
    // Search track_metadata for matches in searchable fields (composer,
    // conductor, lyricist, performer, remixer, producer, label, comment,
    // lyrics, isrc, catalog_number). Merge with FTS results.
    let meta_repo = TrackMetadataRepo::new(state.db.clone());
    let meta_matches = meta_repo.search_by_value(&q.q, limit).unwrap_or_default();

    // Collect track IDs already returned by FTS
    let fts_track_ids: std::collections::HashSet<i64> =
        tracks.iter().filter_map(|t| t.id).collect();

    // Build a map of track_id → matched metadata fields
    let mut matched_metadata: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (track_id, key, value) in &meta_matches {
        matched_metadata
            .entry(*track_id)
            .or_default()
            .insert(key.clone(), value.clone());
    }

    // Fetch tracks that matched via metadata but not via FTS
    let extra_ids: Vec<i64> = meta_matches
        .iter()
        .map(|(id, _, _)| *id)
        .filter(|id| !fts_track_ids.contains(id))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let extra_tracks = if extra_ids.is_empty() {
        Vec::new()
    } else {
        TrackRepo::new(state.db)
            .get_multiple(&extra_ids)
            .unwrap_or_default()
    };

    // Build track JSON: FTS tracks first, then metadata-only tracks.
    // Annotate with matched_metadata where applicable.
    let mut track_results: Vec<Value> = Vec::with_capacity(tracks.len() + extra_tracks.len());
    for t in tracks.iter().chain(extra_tracks.iter()) {
        let mut v = t.to_json();
        if let Some(id) = t.id {
            if let Some(meta) = matched_metadata.get(&id) {
                v.as_object_mut()
                    .unwrap()
                    .insert("matched_metadata".into(), json!(meta));
            }
        }
        track_results.push(v);
    }

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": track_results,
    }))
}
