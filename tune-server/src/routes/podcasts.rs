use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct Subscribe {
    feed_url: String,
    title: String,
    author: Option<String>,
    image_url: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_podcasts))
        .route("/subscriptions", get(list_subscriptions).post(subscribe))
        .route("/subscriptions/{id}", axum::routing::delete(unsubscribe))
        .route("/radiofrance", get(radiofrance_podcasts))
        .route("/episodes/{podcast_id}", get(podcast_episodes))
}

async fn search_podcasts(Query(q): Query<SearchQuery>) -> Json<Value> {
    Json(json!({
        "query": q.q,
        "items": [],
        "message": "podcast search not yet connected to external API",
    }))
}

async fn list_subscriptions(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT id, feed_url, title, author, image_url, description FROM podcast_subscriptions ORDER BY title")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "feed_url": row.get::<_, Option<String>>(1).ok().flatten(),
                    "title": row.get::<_, Option<String>>(2).ok().flatten(),
                    "author": row.get::<_, Option<String>>(3).ok().flatten(),
                    "image_url": row.get::<_, Option<String>>(4).ok().flatten(),
                    "description": row.get::<_, Option<String>>(5).ok().flatten(),
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

async fn subscribe(
    State(state): State<AppState>,
    Json(body): Json<Subscribe>,
) -> impl IntoResponse {
    match state.db.execute(
        "INSERT OR IGNORE INTO podcast_subscriptions (feed_url, title, author, image_url) VALUES (?, ?, ?, ?)",
        &[
            &body.feed_url as &dyn rusqlite::types::ToSql,
            &body.title,
            &body.author,
            &body.image_url,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn unsubscribe(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state.db.execute("DELETE FROM podcast_subscriptions WHERE id = ?", &[&id]).ok();
    StatusCode::NO_CONTENT
}

async fn radiofrance_podcasts() -> Json<Value> {
    Json(json!([
        {"id": "fip-a-la-carte", "name": "FIP \u{00e0} la carte", "station": "FIP"},
        {"id": "club-jazzafip", "name": "Club Jazzafip", "station": "FIP"},
        {"id": "monde-imaginaire", "name": "Le monde imaginaire de...", "station": "FIP"},
        {"id": "fip-pop", "name": "FIP Pop", "station": "FIP"},
        {"id": "france-musique-classique", "name": "Classique mais pas ringard", "station": "France Musique"},
        {"id": "france-culture-fictions", "name": "Fictions", "station": "France Culture"}
    ]))
}

async fn podcast_episodes(Path(podcast_id): Path<String>) -> Json<Value> {
    // Look up the podcast's RSS feed URL from subscriptions, then parse RSS
    // For now, return a stub until RSS parsing is connected
    Json(json!({
        "podcast_id": podcast_id,
        "episodes": [],
        "message": "Episode fetching from RSS not yet implemented"
    }))
}
