use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::radio_repo::RadioRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::state::AppState;

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<i64>,
    sources: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(federated_search))
}

async fn federated_search(
    State(state): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);

    let artists = ArtistRepo::with_backend(state.backend.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let radios = RadioRepo::with_backend(state.backend.clone())
        .search(&p.q)
        .unwrap_or_default();

    // --- Extended metadata search ---
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());
    let meta_matches = meta_repo.search_by_value(&p.q, limit).unwrap_or_default();

    let fts_track_ids: std::collections::HashSet<i64> =
        tracks.iter().filter_map(|t| t.id).collect();

    let mut matched_metadata: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (track_id, key, value) in &meta_matches {
        matched_metadata
            .entry(*track_id)
            .or_default()
            .insert(key.clone(), value.clone());
    }

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

    // Build track JSON with matched_metadata annotations
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

    let requested_sources: Option<Vec<String>> = p
        .sources
        .map(|s| s.split(',').map(|s| s.trim().to_string()).collect());

    let mut service_results: serde_json::Map<String, Value> = serde_json::Map::new();

    {
        let registry = state.services.lock().await;
        for svc_name in registry.list() {
            if let Some(ref sources) = requested_sources
                && !sources.contains(&svc_name)
                && !sources.contains(&"all".to_string())
            {
                continue;
            }

            if let Some(svc) = registry.get(&svc_name) {
                let svc = svc.lock().await;
                if !svc.auth_status().await.authenticated {
                    continue;
                }
                if let Ok(results) = svc.search(&p.q, limit as usize).await {
                    service_results.insert(svc_name, json!(results));
                }
            }
        }
    }

    Json(json!({
        "local": {
            "artists": artists,
            "albums": albums,
            "tracks": track_results,
        },
        "radios": radios,
        "services": service_results,
    }))
}
