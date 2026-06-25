use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::track_repo::TrackRepo;
use tune_core::license::Feature;
use tune_core::lyrics;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{track_id}", get(get_lyrics_for_track))
        .route("/search", get(search_lyrics))
}

/// GET /lyrics/{track_id}
///
/// Load the track from DB to get title/artist/duration, then fetch
/// lyrics (cache-first, fallback LRCLIB).
///
/// Free tier: plain lyrics only.
/// Premium tier: synced lines + plain text.
async fn get_lyrics_for_track(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(track_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db error: {e}")})),
            )
                .into_response();
        }
    };

    let title = &track.title;
    let artist = track.artist_name.as_deref().unwrap_or("Unknown");

    let result = lyrics::get_lyrics(
        &state.backend,
        &state.http_client,
        track_id,
        title,
        artist,
        track.duration_ms,
    )
    .await;

    match result {
        Ok(ly) => {
            let is_premium = state.license.check_feature(Feature::SyncedLyrics).await;

            if is_premium {
                Json(json!({
                    "track_id": track_id,
                    "synced": ly.synced,
                    "lines": ly.lines,
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                }))
                .into_response()
            } else {
                // Free tier: plain text only, no synced lines.
                Json(json!({
                    "track_id": track_id,
                    "synced": false,
                    "lines": [],
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                    "premium_required": ly.synced,
                }))
                .into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("lyrics fetch failed: {e}")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    title: String,
    artist: String,
    duration: Option<i64>,
}

/// GET /lyrics/search?title=X&artist=Y&duration=Z
///
/// Search LRCLIB directly (no track_id, no caching).
async fn search_lyrics(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let result = lyrics::fetch_from_lrclib(
        &state.http_client,
        &params.artist,
        &params.title,
        params.duration,
    )
    .await;

    match result {
        Ok(ly) => {
            let is_premium = state.license.check_feature(Feature::SyncedLyrics).await;

            if is_premium {
                Json(json!({
                    "synced": ly.synced,
                    "lines": ly.lines,
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                }))
                .into_response()
            } else {
                Json(json!({
                    "synced": false,
                    "lines": [],
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                    "premium_required": ly.synced,
                }))
                .into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("lyrics search failed: {e}")})),
        )
            .into_response(),
    }
}
