use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

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

async fn unsubscribe(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    state
        .db
        .execute("DELETE FROM podcast_subscriptions WHERE id = ?", &[&id])
        .ok();
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

async fn podcast_episodes(
    State(state): State<AppState>,
    Path(podcast_id): Path<String>,
) -> impl IntoResponse {
    // Try to find feed URL from subscriptions (by ID first, then by title slug)
    let feed_url = {
        let conn = state.db.connection().lock().unwrap();
        if let Ok(id) = podcast_id.parse::<i64>() {
            conn.query_row(
                "SELECT feed_url FROM podcast_subscriptions WHERE id = ?",
                rusqlite::params![id],
                |row| row.get::<_, String>(0),
            )
            .ok()
        } else {
            conn.query_row(
                "SELECT feed_url FROM podcast_subscriptions WHERE title LIKE ?",
                rusqlite::params![format!("%{}%", podcast_id.replace('-', " "))],
                |row| row.get::<_, String>(0),
            )
            .ok()
        }
    };

    let Some(feed_url) = feed_url else {
        return Json(json!({
            "podcast_id": podcast_id,
            "episodes": [],
            "error": "podcast not found in subscriptions"
        }))
        .into_response();
    };

    // Fetch RSS feed
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    let xml = match client.get(&feed_url).send().await {
        Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
        _ => {
            return Json(json!({"error": "failed to fetch RSS feed"})).into_response();
        }
    };

    // Parse RSS XML using quick-xml
    let episodes = parse_rss_episodes(&xml);
    let count = episodes.len();

    Json(json!({
        "podcast_id": podcast_id,
        "feed_url": feed_url,
        "episodes": episodes,
        "count": count,
    }))
    .into_response()
}

fn parse_rss_episodes(xml: &str) -> Vec<Value> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(xml);
    let mut episodes = Vec::new();
    let mut in_item = false;
    let mut current_tag = String::new();
    let mut title = String::new();
    let mut description = String::new();
    let mut pub_date = String::new();
    let mut audio_url = String::new();
    let mut duration = String::new();
    let mut image_url = String::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "item" || tag == "entry" {
                    in_item = true;
                    title.clear();
                    description.clear();
                    pub_date.clear();
                    audio_url.clear();
                    duration.clear();
                    image_url.clear();
                }
                if in_item {
                    current_tag = tag.clone();
                    // Check for enclosure tag (audio URL)
                    if tag == "enclosure" {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(&attr.value).to_string();
                            if key == "url" {
                                audio_url = val;
                            }
                        }
                    }
                    // itunes:image
                    if tag == "itunes:image" {
                        for attr in e.attributes().flatten() {
                            let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                            if key == "href" {
                                image_url = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) if in_item => {
                let text = e.unescape().unwrap_or_default().to_string();
                match current_tag.as_str() {
                    "title" => title = text,
                    "description" | "summary" => {
                        if description.is_empty() {
                            description = text;
                        }
                    }
                    "pubDate" | "published" => pub_date = text,
                    "itunes:duration" => duration = text,
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) if in_item => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                // Handle self-closing <enclosure ... /> tags
                if tag == "enclosure" {
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        let val = String::from_utf8_lossy(&attr.value).to_string();
                        if key == "url" {
                            audio_url = val;
                        }
                    }
                }
                // Handle self-closing <itunes:image ... /> tags
                if tag == "itunes:image" {
                    for attr in e.attributes().flatten() {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        if key == "href" {
                            image_url = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if (tag == "item" || tag == "entry") && in_item {
                    if !title.is_empty() {
                        episodes.push(json!({
                            "title": title,
                            "description": description.chars().take(500).collect::<String>(),
                            "published": pub_date,
                            "audio_url": audio_url,
                            "duration": duration,
                            "image_url": if image_url.is_empty() { Value::Null } else { Value::String(image_url.clone()) },
                        }));
                    }
                    in_item = false;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    episodes
}
