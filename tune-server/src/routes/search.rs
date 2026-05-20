use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::radio_repo::RadioRepo;

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

    let artists = ArtistRepo::new(state.db.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::new(state.db.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let tracks = TrackRepo::new(state.db.clone())
        .search(&p.q, limit)
        .unwrap_or_default();
    let radios = RadioRepo::new(state.db)
        .search(&p.q)
        .unwrap_or_default();

    let requested_sources: Option<Vec<String>> = p.sources.map(|s| {
        s.split(',').map(|s| s.trim().to_string()).collect()
    });

    let mut service_results: serde_json::Map<String, Value> = serde_json::Map::new();

    {
        let registry = state.services.lock().await;
        for svc_name in registry.list() {
            if let Some(ref sources) = requested_sources
                && !sources.contains(&svc_name) && !sources.contains(&"all".to_string()) {
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
            "tracks": tracks,
        },
        "radios": radios,
        "services": service_results,
    }))
}
