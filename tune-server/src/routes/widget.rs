use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use tune_core::db::history_repo::HistoryRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/data", get(widget_data))
        .route("/now-playing", get(widget_now_playing))
        .route("/quick-actions", get(widget_quick_actions))
        .route("/recent", get(widget_recent))
}

#[derive(Deserialize)]
struct WidgetParams {
    zone_id: Option<String>,
}

async fn widget_data(
    State(state): State<AppState>,
    Query(params): Query<WidgetParams>,
) -> Json<Value> {
    let playback = state.playback.clone();
    let zone_id_num = params.zone_id.as_deref().and_then(|z| z.parse::<i64>().ok()).unwrap_or(1);
    let zone_state = playback.get_state(zone_id_num).await;
    let all_zones = playback.all_states().await;

    // Recent tracks from history
    let repo = HistoryRepo::new(state.db.clone());
    let recent: Vec<Value> = repo
        .top_tracks(5)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({"title": title, "artist_name": artist, "plays": plays})
        })
        .collect();

    Json(json!({
        "now_playing": zone_state.now_playing,
        "state": zone_state.state,
        "zones": all_zones,
        "recent": recent,
        "server_uptime_secs": state.started_at.elapsed().as_secs(),
    }))
}

async fn widget_now_playing(
    State(state): State<AppState>,
    Query(params): Query<WidgetParams>,
) -> Json<Value> {
    let playback = state.playback.clone();
    let zone_id_num = params.zone_id.as_deref().and_then(|z| z.parse::<i64>().ok()).unwrap_or(1);
    let zone_state = playback.get_state(zone_id_num).await;

    let playing = zone_state.state == tune_core::playback::PlayState::Playing;
    match zone_state.now_playing {
        Some(np) => Json(json!({
            "playing": playing,
            "zone_id": params.zone_id,
            "title": np.title,
            "artist_name": np.artist_name,
            "album_title": np.album_title,
            "cover_url": np.cover_path,
            "progress_ms": zone_state.position_ms,
            "duration_ms": np.duration_ms,
        })),
        None => Json(json!({
            "playing": false,
            "zone_id": params.zone_id,
        })),
    }
}

async fn widget_quick_actions() -> Json<Value> {
    Json(json!([
        {
            "id": "shuffle_library",
            "label": "Shuffle Library",
            "icon": "shuffle",
            "action": "/api/v1/playback/shuffle-all",
            "method": "POST",
        },
        {
            "id": "play_favorites",
            "label": "Play Favorites",
            "icon": "heart",
            "action": "/api/v1/playlists/favorites/play",
            "method": "POST",
        },
        {
            "id": "play_radio",
            "label": "Play Radio",
            "icon": "radio",
            "action": "/api/v1/radios/favorites/play-first",
            "method": "POST",
        },
        {
            "id": "recent_albums",
            "label": "Recent Albums",
            "icon": "clock",
            "action": "/api/v1/library/albums?sort=recent&limit=10",
            "method": "GET",
        },
    ]))
}

#[derive(Deserialize)]
struct RecentParams {
    limit: Option<i64>,
}

async fn widget_recent(
    State(state): State<AppState>,
    Query(params): Query<RecentParams>,
) -> Json<Value> {
    let limit = params.limit.unwrap_or(10);
    let repo = HistoryRepo::new(state.db.clone());

    let tracks: Vec<Value> = repo
        .top_tracks(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({
                "title": title,
                "artist_name": artist,
                "plays": plays,
            })
        })
        .collect();

    let albums: Vec<Value> = repo
        .top_albums(limit)
        .unwrap_or_default()
        .into_iter()
        .map(|(title, artist, plays)| {
            json!({
                "album_title": title,
                "artist_name": artist,
                "plays": plays,
            })
        })
        .collect();

    Json(json!({
        "recent_tracks": tracks,
        "recent_albums": albums,
    }))
}
