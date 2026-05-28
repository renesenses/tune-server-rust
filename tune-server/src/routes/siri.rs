use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/shortcuts", get(list_shortcuts))
        .route("/shortcuts/play-radio", post(siri_play_radio))
        .route("/shortcuts/play-playlist", post(siri_play_playlist))
        .route("/shortcuts/play-album", post(siri_play_album))
        .route("/voice-command", post(voice_command))
}

/// List available Siri Shortcuts with their URL schemes.
async fn list_shortcuts(State(state): State<AppState>) -> Json<Value> {
    let base_url = format!("http://localhost:{}", state.port);
    Json(json!({
        "shortcuts": [
            {
                "id": "play-radio",
                "name": "Play Radio",
                "description": "Play a radio station by name",
                "url": format!("{base_url}/api/v1/siri/shortcuts/play-radio"),
                "parameters": {"name": "string — radio station name or genre"},
                "siri_phrase": "Play jazz radio on Tune",
            },
            {
                "id": "play-playlist",
                "name": "Play Playlist",
                "description": "Play a playlist by name",
                "url": format!("{base_url}/api/v1/siri/shortcuts/play-playlist"),
                "parameters": {"name": "string — playlist name"},
                "siri_phrase": "Play my favorites on Tune",
            },
            {
                "id": "play-album",
                "name": "Play Album",
                "description": "Play an album by title and/or artist",
                "url": format!("{base_url}/api/v1/siri/shortcuts/play-album"),
                "parameters": {"title": "string", "artist": "string (optional)"},
                "siri_phrase": "Play Kind of Blue on Tune",
            },
            {
                "id": "voice-command",
                "name": "Voice Command",
                "description": "Parse a natural language command",
                "url": format!("{base_url}/api/v1/siri/shortcuts/voice-command"),
                "parameters": {"text": "string — natural language command"},
                "siri_phrase": "Tell Tune to play something relaxing",
            },
        ],
    }))
}

#[derive(Deserialize)]
struct PlayRadioBody {
    name: String,
    zone_id: Option<String>,
}

/// Play a radio station matching the given name.
async fn siri_play_radio(
    State(state): State<AppState>,
    Json(body): Json<PlayRadioBody>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();

    let pattern = format!("%{}%", body.name);
    let station: Option<Value> = conn
        .prepare("SELECT id, name, url FROM radios WHERE name LIKE ?1 LIMIT 1")
        .and_then(|mut stmt| {
            stmt.query_row([&pattern], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                    "url": row.get::<_, Option<String>>(2)?,
                }))
            })
        })
        .ok();
    drop(conn);

    match station {
        Some(s) => Json(json!({
            "action": "play_radio",
            "station": s,
            "zone_id": body.zone_id,
            "status": "queued",
        })),
        None => Json(json!({
            "action": "play_radio",
            "error": format!("No radio station matching '{}'", body.name),
            "status": "not_found",
        })),
    }
}

#[derive(Deserialize)]
struct PlayPlaylistBody {
    name: String,
    zone_id: Option<String>,
}

/// Play a playlist matching the given name.
async fn siri_play_playlist(
    State(state): State<AppState>,
    Json(body): Json<PlayPlaylistBody>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();

    let pattern = format!("%{}%", body.name);
    let playlist: Option<Value> = conn
        .prepare("SELECT id, name FROM playlists WHERE name LIKE ?1 LIMIT 1")
        .and_then(|mut stmt| {
            stmt.query_row([&pattern], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, Option<String>>(1)?,
                }))
            })
        })
        .ok();
    drop(conn);

    match playlist {
        Some(p) => Json(json!({
            "action": "play_playlist",
            "playlist": p,
            "zone_id": body.zone_id,
            "status": "queued",
        })),
        None => Json(json!({
            "action": "play_playlist",
            "error": format!("No playlist matching '{}'", body.name),
            "status": "not_found",
        })),
    }
}

#[derive(Deserialize)]
struct PlayAlbumBody {
    title: String,
    artist: Option<String>,
    zone_id: Option<String>,
}

/// Play an album matching the given title (and optionally artist).
async fn siri_play_album(
    State(state): State<AppState>,
    Json(body): Json<PlayAlbumBody>,
) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();

    let title_pattern = format!("%{}%", body.title);
    let album: Option<Value> = if let Some(ref artist) = body.artist {
        let artist_pattern = format!("%{artist}%");
        conn.prepare(
            "SELECT id, title, artist_name FROM albums WHERE title LIKE ?1 AND artist_name LIKE ?2 LIMIT 1",
        )
        .and_then(|mut stmt| {
            stmt.query_row([&title_pattern, &artist_pattern], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, Option<String>>(1)?,
                    "artist_name": row.get::<_, Option<String>>(2)?,
                }))
            })
        })
        .ok()
    } else {
        conn.prepare("SELECT id, title, artist_name FROM albums WHERE title LIKE ?1 LIMIT 1")
            .and_then(|mut stmt| {
                stmt.query_row([&title_pattern], |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "title": row.get::<_, Option<String>>(1)?,
                        "artist_name": row.get::<_, Option<String>>(2)?,
                    }))
                })
            })
            .ok()
    };
    drop(conn);

    match album {
        Some(a) => Json(json!({
            "action": "play_album",
            "album": a,
            "zone_id": body.zone_id,
            "status": "queued",
        })),
        None => Json(json!({
            "action": "play_album",
            "error": format!("No album matching '{}'", body.title),
            "status": "not_found",
        })),
    }
}

#[derive(Deserialize)]
struct VoiceCommandBody {
    text: String,
    zone_id: Option<String>,
}

/// Parse a natural language voice command and execute the appropriate action.
async fn voice_command(
    State(state): State<AppState>,
    Json(body): Json<VoiceCommandBody>,
) -> Json<Value> {
    let text = body.text.to_lowercase();

    // Simple keyword-based NLU
    if text.contains("pause") || text.contains("stop") {
        return Json(json!({
            "action": "pause",
            "zone_id": body.zone_id,
            "parsed": "pause playback",
            "status": "executed",
        }));
    }

    if text.contains("resume") || text.starts_with("play") && text.len() < 6 {
        return Json(json!({
            "action": "resume",
            "zone_id": body.zone_id,
            "parsed": "resume playback",
            "status": "executed",
        }));
    }

    if text.contains("next") || text.contains("skip") {
        return Json(json!({
            "action": "next",
            "zone_id": body.zone_id,
            "parsed": "skip to next track",
            "status": "executed",
        }));
    }

    if text.contains("previous") || text.contains("back") {
        return Json(json!({
            "action": "previous",
            "zone_id": body.zone_id,
            "parsed": "go to previous track",
            "status": "executed",
        }));
    }

    if text.contains("shuffle") {
        return Json(json!({
            "action": "shuffle",
            "zone_id": body.zone_id,
            "parsed": "shuffle playback",
            "status": "executed",
        }));
    }

    // "play X" -> search and play
    if let Some(query) = text.strip_prefix("play ") {
        let query = query.trim();
        // Search the library
        let conn = state.db.connection().lock().unwrap();
        let pattern = format!("%{query}%");

        let track: Option<Value> = conn
            .prepare("SELECT id, title, artist_name FROM tracks WHERE title LIKE ?1 OR artist_name LIKE ?1 LIMIT 1")
            .and_then(|mut stmt| {
                stmt.query_row([&pattern], |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "title": row.get::<_, Option<String>>(1)?,
                        "artist_name": row.get::<_, Option<String>>(2)?,
                    }))
                })
            })
            .ok();
        drop(conn);

        return Json(json!({
            "action": "play_search",
            "query": query,
            "result": track,
            "zone_id": body.zone_id,
            "parsed": format!("search and play: {query}"),
            "status": if track.is_some() { "found" } else { "not_found" },
        }));
    }

    Json(json!({
        "action": "unknown",
        "text": body.text,
        "parsed": null,
        "status": "unrecognized",
        "message": "Could not parse voice command. Try: 'play [artist/album/track]', 'pause', 'next', 'shuffle'.",
    }))
}
