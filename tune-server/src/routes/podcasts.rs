use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::playback::NowPlaying;
use tune_core::streaming::podcasts::PodcastService;
use tune_core::streaming::radiofrance::{RadioFranceApi, RfStation};
#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_country")]
    country: String,
    language: Option<String>,
}
fn default_limit() -> usize {
    20
}
fn default_country() -> String {
    "us".into()
}
#[derive(Deserialize)]
struct TopQuery {
    genre: Option<u32>,
    #[serde(default = "default_country")]
    country: String,
}
#[derive(Deserialize)]
struct EpisodesQuery {
    feed_url: Option<String>,
    #[serde(default = "default_episode_limit")]
    limit: usize,
}
fn default_episode_limit() -> usize {
    50
}
#[derive(Deserialize)]
struct Subscribe {
    #[serde(default)]
    feed_url: String,
    title: String,
    author: Option<String>,
    image_url: Option<String>,
    description: Option<String>,
    source_id: Option<String>,
}
#[derive(Deserialize)]
struct PlayEpisodeRequest {
    audio_url: String,
    title: Option<String>,
    podcast_name: Option<String>,
    cover_url: Option<String>,
    duration_ms: Option<u64>,
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_podcasts))
        .route("/subscriptions", get(list_subscriptions).post(subscribe))
        .route("/subscriptions/{id}", axum::routing::delete(unsubscribe))
        .route("/radiofrance", get(radiofrance_podcasts))
        .route("/radiofrance/shows", get(rf_shows))
        .route("/radiofrance/shows/search", get(rf_search_shows))
        .route("/radiofrance/episodes", get(rf_episodes))
        .route("/discover", get(discover_podcasts))
        .route("/top", get(top_podcasts))
        .route("/genres", get(list_genres))
        .route("/episodes/{podcast_id}", get(podcast_episodes))
        .route("/episodes", get(episodes_by_feed_url))
        .route("/play/{zone_id}", post(play_episode))
}
async fn search_podcasts(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc
        .search(&q.q, q.limit, &q.country, q.language.as_deref())
        .await
    {
        Ok(results) => Ok(Json(
            json!({"query": q.q, "count": results.len(), "items": results}),
        )),
        Err(e) => {
            warn!(query = %q.q, error = %e, "podcast_search_failed");
            Err(AppError::internal(e))
        }
    }
}
async fn list_subscriptions(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many("SELECT id, feed_url, title, author, image_url, description FROM podcast_subscriptions ORDER BY title", &[]).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows.into_iter().map(|r| json!({"id": r.get(0).and_then(|v| v.as_i64()), "feed_url": r.get(1).and_then(|v| v.as_string()), "title": r.get(2).and_then(|v| v.as_string()), "author": r.get(3).and_then(|v| v.as_string()), "image_url": r.get(4).and_then(|v| v.as_string()), "description": r.get(5).and_then(|v| v.as_string())})).collect();
    Ok(Json(json!(items)))
}
async fn subscribe(
    State(state): State<AppState>,
    Json(body): Json<Subscribe>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;

    let feed_url = if body.feed_url.is_empty() {
        // Top chart podcasts have no feed URL — resolve via iTunes lookup.
        let apple_id = body
            .source_id
            .as_deref()
            .and_then(|s| s.strip_prefix("apple-"))
            .unwrap_or("");
        if apple_id.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "No feed URL and no Apple ID to resolve"})),
            )
                .into_response();
        }
        let svc = PodcastService::with_client(state.http_client.clone());
        match svc.resolve_feed_url(apple_id).await {
            Some(url) => url,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "Could not resolve feed URL from Apple ID"})),
                )
                    .into_response();
            }
        }
    } else {
        body.feed_url.clone()
    };

    let sql = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "INSERT INTO podcast_subscriptions (feed_url, title, author, image_url, description) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (feed_url) DO NOTHING"
    } else {
        "INSERT OR IGNORE INTO podcast_subscriptions (feed_url, title, author, image_url, description) VALUES (?, ?, ?, ?, ?)"
    };
    match state.backend.execute(
        sql,
        &[
            &feed_url as &dyn ToSqlValue,
            &body.title as &dyn ToSqlValue,
            &body.author as &dyn ToSqlValue,
            &body.image_url as &dyn ToSqlValue,
            &body.description as &dyn ToSqlValue,
        ],
    ) {
        Ok(_) => {
            info!(title = %body.title, feed_url = %feed_url, "podcast_subscribed");
            (
                StatusCode::CREATED,
                Json(json!({"title": body.title, "feed_url": feed_url})),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
async fn unsubscribe(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "DELETE FROM podcast_subscriptions WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}
async fn radiofrance_podcasts() -> Json<Value> {
    Json(json!(PodcastService::curated_french_podcasts()))
}
/// GET /discover — curated French podcasts + optional Apple top chart enrichment.
async fn discover_podcasts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let curated = PodcastService::curated_french_podcasts();
    let svc = PodcastService::with_client(state.http_client.clone());
    let top = svc.top_podcasts(None, "us").await.unwrap_or_default();
    Ok(Json(json!({
        "curated": curated,
        "top": top,
        "genres": PodcastService::available_genres(),
    })))
}
/// GET /top?genre={genreId} — Apple Top 50 podcasts in France, optionally filtered by genre.
async fn top_podcasts(
    State(state): State<AppState>,
    Query(q): Query<TopQuery>,
) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.top_podcasts(q.genre, &q.country).await {
        Ok(podcasts) => Ok(Json(json!({
            "genre": q.genre,
            "count": podcasts.len(),
            "items": podcasts,
        }))),
        Err(e) => {
            warn!(genre = ?q.genre, error = %e, "top_podcasts_failed");
            Err(AppError::internal(e))
        }
    }
}
/// GET /genres — list all available genre filters for the top endpoint.
async fn list_genres() -> Json<Value> {
    Json(json!(PodcastService::available_genres()))
}
async fn episodes_by_feed_url(
    State(state): State<AppState>,
    Query(q): Query<EpisodesQuery>,
) -> Result<Json<Value>, AppError> {
    let Some(feed_url) = q.feed_url else {
        return Err(AppError::bad_request(
            "feed_url query parameter is required",
        ));
    };
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.get_episodes(&feed_url, q.limit).await {
        Ok(episodes) => Ok(Json(
            json!({"feed_url": feed_url, "count": episodes.len(), "episodes": episodes}),
        )),
        Err(e) => {
            warn!(feed_url = %feed_url, error = %e, "podcast_episodes_fetch_failed");
            Err(AppError::internal(e))
        }
    }
}
async fn podcast_episodes(
    State(state): State<AppState>,
    Path(podcast_id): Path<String>,
    Query(q): Query<EpisodesQuery>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(ref feed_url) = q.feed_url {
        let svc = PodcastService::with_client(state.http_client.clone());
        return match svc.get_episodes(feed_url, q.limit).await {
            Ok(episodes) => Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": episodes.len(), "episodes": episodes})).into_response()),
            Err(e) => Ok(Json(json!({"podcast_id": podcast_id, "error": e})).into_response()),
        };
    }
    let feed_url = {
        use tune_core::db::backend::ToSqlValue;
        if let Ok(id) = podcast_id.parse::<i64>() {
            state
                .backend
                .query_one(
                    "SELECT feed_url FROM podcast_subscriptions WHERE id = ?",
                    &[&id as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .and_then(|r| r.first().and_then(|v| v.as_string()))
        } else {
            let like = format!("%{}%", podcast_id.replace('-', " "));
            state
                .backend
                .query_one(
                    "SELECT feed_url FROM podcast_subscriptions WHERE title LIKE ?",
                    &[&like as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .and_then(|r| r.first().and_then(|v| v.as_string()))
        }
    };
    let Some(feed_url) = feed_url else {
        return Ok(Json(json!({"podcast_id": podcast_id, "episodes": [], "error": "podcast not found in subscriptions"})).into_response());
    };
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.get_episodes(&feed_url, q.limit).await {
        Ok(episodes) => {
            let count = episodes.len();
            Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": count, "episodes": episodes})).into_response())
        }
        Err(e) => Ok(
            Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "error": e}))
                .into_response(),
        ),
    }
}
async fn play_episode(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<PlayEpisodeRequest>,
) -> impl IntoResponse {
    let title = body.title.as_deref().unwrap_or("Podcast Episode");
    let podcast_name = body.podcast_name.as_deref().unwrap_or("Podcast");
    let device_id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    let mime_type = guess_audio_mime(&body.audio_url);
    let np = NowPlaying {
        track_id: None,
        title: title.to_string(),
        artist_name: Some(podcast_name.to_string()),
        album_title: Some(podcast_name.to_string()),
        cover_path: body.cover_url.clone(),
        duration_ms: body.duration_ms.unwrap_or(0) as i64,
        source: "podcast".into(),
        source_id: Some(body.audio_url.clone()),
        stream_id: None,
        ..Default::default()
    };
    state.playback.play(zone_id, np).await;
    let (output_sent, output_error) = if let Some(ref did) = device_id {
        let output_arc = {
            let outputs = state.outputs.lock().await;
            outputs.get(did)
        };
        if let Some(output_arc) = output_arc {
            let output = output_arc.lock().await;
            let media = tune_core::outputs::PlayMedia {
                url: &body.audio_url,
                mime_type: &mime_type,
                title: Some(title),
                artist: Some(podcast_name),
                album: Some(podcast_name),
                cover_url: body.cover_url.as_deref(),
                duration_ms: body.duration_ms,
                ..Default::default()
            };
            match output.play_media(&media).await {
                Ok(()) => (true, None),
                Err(e) => (false, Some(format!("Output device error: {e}"))),
            }
        } else {
            (
                false,
                Some("Device not yet discovered. Please retry in a few seconds.".into()),
            )
        }
    } else {
        (false, None)
    };
    info!(
        zone_id,
        title,
        podcast = podcast_name,
        output_sent,
        "podcast_episode_play"
    );
    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({"zone_id": zone_id, "title": title, "podcast": podcast_name, "audio_url": body.audio_url, "mime_type": mime_type, "output_sent": output_sent, "error": output_error, "state": zone_state})).into_response()
}
// ─── Radio France GraphQL API ───────────────────────────────────────

#[derive(Deserialize)]
struct RfShowsQuery {
    station: Option<String>,
}

#[derive(Deserialize)]
struct RfSearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct RfEpisodesQuery {
    show_url: String,
    #[serde(default = "default_episode_limit")]
    limit: usize,
}

fn get_rf_api_key(state: &AppState) -> Option<String> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("radiofrance_api_key")
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
}

async fn rf_shows(
    State(state): State<AppState>,
    Query(q): Query<RfShowsQuery>,
) -> Result<Json<Value>, AppError> {
    let api_key = get_rf_api_key(&state)
        .ok_or_else(|| AppError::bad_request("radiofrance_api_key not configured"))?;
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    let code = q.station.as_deref().unwrap_or("FRANCEINTER");
    let station = RfStation::from_code(code)
        .ok_or_else(|| AppError::bad_request(&format!("unknown station: {code}")))?;
    match api.list_shows(station).await {
        Ok(shows) => Ok(Json(
            json!({"station": station.label(), "count": shows.len(), "shows": shows}),
        )),
        Err(e) => {
            warn!(station = code, error = %e, "rf_shows_failed");
            Err(AppError::internal(e))
        }
    }
}

async fn rf_search_shows(
    State(state): State<AppState>,
    Query(q): Query<RfSearchQuery>,
) -> Result<Json<Value>, AppError> {
    let api_key = get_rf_api_key(&state)
        .ok_or_else(|| AppError::bad_request("radiofrance_api_key not configured"))?;
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    match api.search_shows(&q.q).await {
        Ok(shows) => Ok(Json(
            json!({"query": q.q, "count": shows.len(), "shows": shows}),
        )),
        Err(e) => {
            warn!(query = %q.q, error = %e, "rf_search_failed");
            Err(AppError::internal(e))
        }
    }
}

async fn rf_episodes(
    State(state): State<AppState>,
    Query(q): Query<RfEpisodesQuery>,
) -> Result<Json<Value>, AppError> {
    let api_key = get_rf_api_key(&state)
        .ok_or_else(|| AppError::bad_request("radiofrance_api_key not configured"))?;
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    match api.get_episodes(&q.show_url, q.limit as u32).await {
        Ok(episodes) => Ok(Json(
            json!({"show_url": q.show_url, "count": episodes.len(), "episodes": episodes}),
        )),
        Err(e) => {
            warn!(show = %q.show_url, error = %e, "rf_episodes_failed");
            Err(AppError::internal(e))
        }
    }
}

fn guess_audio_mime(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") {
        "audio/mpeg"
    } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") {
        "audio/mp4"
    } else if path.ends_with(".ogg") || path.ends_with(".opus") {
        "audio/ogg"
    } else if path.ends_with(".flac") {
        "audio/flac"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_guess_audio_mime() {
        assert_eq!(guess_audio_mime("https://x.com/ep.mp3"), "audio/mpeg");
        assert_eq!(guess_audio_mime("https://x.com/ep.m4a?t=1"), "audio/mp4");
        assert_eq!(guess_audio_mime("https://x.com/stream"), "audio/mpeg");
    }
}
