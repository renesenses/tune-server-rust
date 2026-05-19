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

    Json(json!({
        "local": {
            "artists": artists,
            "albums": albums,
            "tracks": tracks,
        },
        "radios": radios,
        "services": {},
    }))
}
