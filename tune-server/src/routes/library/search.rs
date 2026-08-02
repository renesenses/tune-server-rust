use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use super::SearchQuery;

/// Body for `POST /library/search/acoustic` — natural-language acoustic search.
/// Its fields are only read by the real handler; the no-feature stub ignores the
/// body, so silence dead-code there rather than emit a build warning.
#[derive(Deserialize)]
#[cfg_attr(not(feature = "audio-embedding"), allow(dead_code))]
pub(super) struct AcousticQuery {
    /// Free-text mood/timbre query, e.g. "warm analog jazz", "driving techno".
    pub query: String,
    /// Max tracks to return (clamped 1..=200; default 50).
    pub limit: Option<i64>,
}

pub(super) async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(20);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();

    // --- Extended metadata search (Approach B) ---
    // Search track_metadata for matches in searchable fields (composer,
    // conductor, lyricist, performer, remixer, producer, label, comment,
    // lyrics, isrc, catalog_number). Merge with FTS results.
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());
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
        TrackRepo::with_backend(state.backend.clone())
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

/// POST /library/search/acoustic — natural-language acoustic search (Phase 3).
///
/// Embeds the query with the CLAP text tower (same joint space as the library's
/// audio embeddings) and ranks tracks by cosine similarity — so "warm analog
/// jazz" surfaces acoustically matching tracks whatever their tags say. Premium.
/// Requires an `audio-embedding` build with the text model provisioned; returns
/// 503 when the model can't be loaded, and an empty list when no track has been
/// acoustically analysed yet (the sweep hasn't run / produced vectors).
#[cfg(feature = "audio-embedding")]
pub(super) async fn acoustic_search(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<AcousticQuery>,
) -> Result<Json<Value>, crate::error::AppError> {
    use tune_core::audio::{embedding_store, text_embedding};
    use tune_core::license::Feature;

    if !state
        .license
        .check_feature(Feature::AiRecommendations)
        .await
    {
        return Ok(Json(json!({
            "error": "premium_required",
            "message": "Acoustic search requires a Premium license",
            "tracks": [],
            "count": 0,
        })));
    }

    let query = body.query.trim();
    if query.is_empty() {
        return Err(crate::error::AppError::bad_request(
            "query must not be empty",
        ));
    }
    let limit = body.limit.unwrap_or(50).clamp(1, 200) as usize;

    // Text-tower embedding (provisions runtime + model + tokenizer on first call).
    let qvec = text_embedding::embed_query(&state.backend, query)
        .await
        .map_err(|e| {
            crate::error::AppError::service_unavailable(format!(
                "acoustic search model unavailable: {e}"
            ))
        })?;

    let ranked = embedding_store::rank_by_vector(&state.backend, &qvec, limit, None);
    if ranked.is_empty() {
        return Ok(Json(json!({ "query": query, "tracks": [], "count": 0 })));
    }

    let scores: HashMap<i64, f32> = ranked.iter().copied().collect();
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&ids)
        .unwrap_or_default();

    // get_multiple preserves the ranked order; annotate each with its cosine.
    let items: Vec<Value> = tracks
        .iter()
        .map(|t| {
            let mut v = t.to_json();
            if let (Some(obj), Some(id)) = (v.as_object_mut(), t.id) {
                if let Some(s) = scores.get(&id) {
                    obj.insert("similarity".into(), json!(s));
                }
            }
            v
        })
        .collect();

    Ok(Json(json!({
        "query": query,
        "count": items.len(),
        "tracks": items,
    })))
}

/// Stub for builds without the `audio-embedding` feature: the text tower isn't
/// linked, so acoustic search is unavailable. Keeps the route present (503) so
/// the web client gets a clear signal rather than a 404.
#[cfg(not(feature = "audio-embedding"))]
pub(super) async fn acoustic_search(
    State(_state): State<AppState>,
    axum::Json(_body): axum::Json<AcousticQuery>,
) -> Result<Json<Value>, crate::error::AppError> {
    Err(crate::error::AppError::service_unavailable(
        "acoustic search is not available in this build",
    ))
}
