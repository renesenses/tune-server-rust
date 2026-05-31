use axum::extract::{Query, State};
use axum::Json;
use serde_json::{json, Value};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;
use crate::state::AppState;

use super::SearchQuery;

pub(super) async fn search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Value> {
    let limit = q.limit.unwrap_or(20);
    let artists = ArtistRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::new(state.db.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    let tracks = TrackRepo::new(state.db)
        .search(&q.q, limit)
        .unwrap_or_default();

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
    }))
}
