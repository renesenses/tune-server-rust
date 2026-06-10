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
use crate::error::AppError;
use crate::state::AppState;
#[derive(Deserialize)]
struct SearchQuery { q: String, #[serde(default = "default_limit")] limit: usize }
fn default_limit() -> usize { 20 }
#[derive(Deserialize)]
struct EpisodesQuery { feed_url: Option<String>, #[serde(default = "default_episode_limit")] limit: usize }
fn default_episode_limit() -> usize { 50 }
#[derive(Deserialize)]
struct Subscribe { feed_url: String, title: String, author: Option<String>, image_url: Option<String>, description: Option<String> }
#[derive(Deserialize)]
struct PlayEpisodeRequest { audio_url: String, title: Option<String>, podcast_name: Option<String>, cover_url: Option<String>, duration_ms: Option<u64> }
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_podcasts))
        .route("/subscriptions", get(list_subscriptions).post(subscribe))
        .route("/subscriptions/{id}", axum::routing::delete(unsubscribe))
        .route("/radiofrance", get(radiofrance_podcasts))
        .route("/episodes/{podcast_id}", get(podcast_episodes))
        .route("/episodes", get(episodes_by_feed_url))
        .route("/play/{zone_id}", post(play_episode))
}
async fn search_podcasts(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.search(&q.q, q.limit).await {
        Ok(results) => Ok(Json(json!({"query": q.q, "count": results.len(), "items": results}))),
        Err(e) => { warn!(query = %q.q, error = %e, "podcast_search_failed"); Err(AppError::internal(e)) }
    }
}
async fn list_subscriptions(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
    let items: Vec<Value> = conn.prepare("SELECT id, feed_url, title, author, image_url, description FROM podcast_subscriptions ORDER BY title").and_then(|mut stmt| { stmt.query_map([], |row| { Ok(json!({"id": row.get::<_, Option<i64>>(0).ok().flatten(), "feed_url": row.get::<_, Option<String>>(1).ok().flatten(), "title": row.get::<_, Option<String>>(2).ok().flatten(), "author": row.get::<_, Option<String>>(3).ok().flatten(), "image_url": row.get::<_, Option<String>>(4).ok().flatten(), "description": row.get::<_, Option<String>>(5).ok().flatten()})) }).and_then(|rows| rows.collect::<Result<Vec<_>, _>>()) }).unwrap_or_default();
    drop(conn);
    Ok(Json(json!(items)))
}
async fn subscribe(State(state): State<AppState>, Json(body): Json<Subscribe>) -> impl IntoResponse {
    match state.db.execute("INSERT OR IGNORE INTO podcast_subscriptions (feed_url, title, author, image_url, description) VALUES (?, ?, ?, ?, ?)", &[&body.feed_url as &dyn rusqlite::types::ToSql, &body.title, &body.author, &body.image_url, &body.description]) {
        Ok(_) => { let id = state.db.last_insert_rowid(); info!(title = %body.title, feed_url = %body.feed_url, "podcast_subscribed"); (StatusCode::CREATED, Json(json!({"id": id, "title": body.title}))).into_response() }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
async fn unsubscribe(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse { state.db.execute("DELETE FROM podcast_subscriptions WHERE id = ?", &[&id]).ok(); StatusCode::NO_CONTENT }
async fn radiofrance_podcasts() -> Json<Value> { Json(json!(PodcastService::radio_france_podcasts())) }
async fn episodes_by_feed_url(State(state): State<AppState>, Query(q): Query<EpisodesQuery>) -> Result<Json<Value>, AppError> {
    let Some(feed_url) = q.feed_url else { return Err(AppError::bad_request("feed_url query parameter is required")); };
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.get_episodes(&feed_url, q.limit).await {
        Ok(episodes) => Ok(Json(json!({"feed_url": feed_url, "count": episodes.len(), "episodes": episodes}))),
        Err(e) => { warn!(feed_url = %feed_url, error = %e, "podcast_episodes_fetch_failed"); Err(AppError::internal(e)) }
    }
}
async fn podcast_episodes(State(state): State<AppState>, Path(podcast_id): Path<String>, Query(q): Query<EpisodesQuery>) -> Result<impl IntoResponse, AppError> {
    if let Some(ref feed_url) = q.feed_url {
        let svc = PodcastService::with_client(state.http_client.clone());
        return match svc.get_episodes(feed_url, q.limit).await {
            Ok(episodes) => Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": episodes.len(), "episodes": episodes})).into_response()),
            Err(e) => Ok(Json(json!({"podcast_id": podcast_id, "error": e})).into_response()),
        };
    }
    let feed_url = { let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?; if let Ok(id) = podcast_id.parse::<i64>() { conn.query_row("SELECT feed_url FROM podcast_subscriptions WHERE id = ?", rusqlite::params![id], |row| row.get::<_, String>(0)).ok() } else { conn.query_row("SELECT feed_url FROM podcast_subscriptions WHERE title LIKE ?", rusqlite::params![format!("%{}%", podcast_id.replace('-', " "))], |row| row.get::<_, String>(0)).ok() } };
    let Some(feed_url) = feed_url else { return Ok(Json(json!({"podcast_id": podcast_id, "episodes": [], "error": "podcast not found in subscriptions"})).into_response()); };
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.get_episodes(&feed_url, q.limit).await {
        Ok(episodes) => { let count = episodes.len(); Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": count, "episodes": episodes})).into_response()) }
        Err(e) => Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "error": e})).into_response()),
    }
}
async fn play_episode(State(state): State<AppState>, Path(zone_id): Path<i64>, Json(body): Json<PlayEpisodeRequest>) -> impl IntoResponse {
    let title = body.title.as_deref().unwrap_or("Podcast Episode");
    let podcast_name = body.podcast_name.as_deref().unwrap_or("Podcast");
    let device_id = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone()).get(zone_id).ok().flatten().and_then(|z| z.output_device_id);
    let mime_type = guess_audio_mime(&body.audio_url);
    let np = NowPlaying { track_id: None, title: title.to_string(), artist_name: Some(podcast_name.to_string()), album_title: Some(podcast_name.to_string()), cover_path: body.cover_url.clone(), duration_ms: body.duration_ms.unwrap_or(0) as i64, source: "podcast".into(), source_id: Some(body.audio_url.clone()), stream_id: None };
    state.playback.play(zone_id, np).await;
    let (output_sent, output_error) = if let Some(ref did) = device_id {
        let output_arc = { let outputs = state.outputs.lock().await; outputs.get(did) };
        if let Some(output_arc) = output_arc { let output = output_arc.lock().await; let media = tune_core::outputs::PlayMedia { url: &body.audio_url, mime_type: &mime_type, title: Some(title), artist: Some(podcast_name), album: Some(podcast_name), cover_url: body.cover_url.as_deref(), duration_ms: body.duration_ms, ..Default::default() }; match output.play_media(&media).await { Ok(()) => (true, None), Err(e) => (false, Some(format!("Output device error: {e}"))) } }
        else { (false, Some("Device not yet discovered. Please retry in a few seconds.".into())) }
    } else { (false, None) };
    info!(zone_id, title, podcast = podcast_name, output_sent, "podcast_episode_play");
    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({"zone_id": zone_id, "title": title, "podcast": podcast_name, "audio_url": body.audio_url, "mime_type": mime_type, "output_sent": output_sent, "error": output_error, "state": zone_state})).into_response()
}
fn guess_audio_mime(url: &str) -> &'static str {
    let lower = url.to_lowercase(); let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") { "audio/mpeg" } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") { "audio/mp4" } else if path.ends_with(".ogg") || path.ends_with(".opus") { "audio/ogg" } else if path.ends_with(".flac") { "audio/flac" } else if path.ends_with(".wav") { "audio/wav" } else { "audio/mpeg" }
}
#[cfg(test)]
mod tests { use super::*; #[test] fn test_guess_audio_mime() { assert_eq!(guess_audio_mime("https://x.com/ep.mp3"), "audio/mpeg"); assert_eq!(guess_audio_mime("https://x.com/ep.m4a?t=1"), "audio/mp4"); assert_eq!(guess_audio_mime("https://x.com/stream"), "audio/mpeg"); } }
