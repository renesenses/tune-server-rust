use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/generate", post(generate_smart_playlist))
        .route("/mood", post(mood_playlist))
        .route("/similar-to", post(similar_to_playlist))
        .route("/history-based", post(history_based_playlist))
        .route("/tempo-match", post(tempo_match_playlist))
        .route("/discovery", post(discovery_playlist))
}

// ---------------------------------------------------------------------------
// Generate from natural language prompt
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GenerateRequest {
    prompt: String,
    limit: Option<i64>,
}

async fn generate_smart_playlist(
    State(state): State<AppState>,
    Json(body): Json<GenerateRequest>,
) -> Result<Json<Value>, AppError> {
    let limit = body.limit.unwrap_or(30);
    let prompt = body.prompt.to_lowercase();

    // Parse keywords from prompt to build SQL conditions
    let mut conditions = Vec::new();
    let mut order_by = "RANDOM()";

    // Detect genre keywords
    let genre_keywords = [
        ("jazz", "jazz"),
        ("classical", "classical"),
        ("rock", "rock"),
        ("pop", "pop"),
        ("electronic", "electronic"),
        ("ambient", "ambient"),
        ("blues", "blues"),
        ("soul", "soul"),
        ("funk", "funk"),
        ("hip hop", "hip hop"),
        ("hip-hop", "hip hop"),
        ("r&b", "r&b"),
        ("country", "country"),
        ("folk", "folk"),
        ("metal", "metal"),
        ("punk", "punk"),
        ("reggae", "reggae"),
        ("disco", "disco"),
        ("latin", "latin"),
        ("world", "world"),
    ];

    for (keyword, genre) in &genre_keywords {
        if prompt.contains(keyword) {
            conditions.push(format!(
                "(t.genre LIKE '%{}%' OR t.genres LIKE '%{}%')",
                genre.replace('\'', "''"),
                genre.replace('\'', "''"),
            ));
        }
    }

    // Detect mood/time-of-day keywords and map to BPM ranges
    if prompt.contains("relax") || prompt.contains("calm") || prompt.contains("chill") {
        conditions.push("(t.bpm IS NULL OR t.bpm BETWEEN 50 AND 100)".into());
        order_by = "t.bpm ASC NULLS LAST";
    }
    if prompt.contains("evening") || prompt.contains("night") || prompt.contains("soir") {
        conditions.push("(t.bpm IS NULL OR t.bpm BETWEEN 50 AND 110)".into());
    }
    if prompt.contains("morning") || prompt.contains("matin") {
        conditions.push("(t.bpm IS NULL OR t.bpm BETWEEN 80 AND 130)".into());
    }
    if prompt.contains("energi") || prompt.contains("workout") || prompt.contains("sport") {
        conditions.push("(t.bpm IS NULL OR t.bpm BETWEEN 120 AND 180)".into());
        order_by = "t.bpm DESC NULLS LAST";
    }
    if prompt.contains("focus") || prompt.contains("study") || prompt.contains("concentr") {
        conditions.push("(t.bpm IS NULL OR t.bpm BETWEEN 70 AND 120)".into());
    }

    // Detect decade keywords
    if prompt.contains("80s") || prompt.contains("eighties") {
        conditions.push("(t.year BETWEEN 1980 AND 1989)".into());
    }
    if prompt.contains("90s") || prompt.contains("nineties") {
        conditions.push("(t.year BETWEEN 1990 AND 1999)".into());
    }
    if prompt.contains("70s") || prompt.contains("seventies") {
        conditions.push("(t.year BETWEEN 1970 AND 1979)".into());
    }
    if prompt.contains("60s") || prompt.contains("sixties") {
        conditions.push("(t.year BETWEEN 1960 AND 1969)".into());
    }
    if prompt.contains("recent") || prompt.contains("new") || prompt.contains("modern") {
        conditions.push("(t.year >= 2015)".into());
    }

    // Detect quality keywords
    if prompt.contains("hi-res") || prompt.contains("hires") || prompt.contains("high res") {
        conditions.push("(t.sample_rate > 48000 OR t.bit_depth > 16)".into());
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path, t.format \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         {where_clause} \
         ORDER BY {order_by} \
         LIMIT ?",
    );

    let tracks: Vec<Value> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                    "format": row.get::<_, Option<String>>(9).ok().flatten(),
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Ok(Json(json!({
        "name": format!("AI: {}", body.prompt),
        "prompt": body.prompt,
        "tracks": tracks,
        "total": tracks.len(),
        "parsed_conditions": conditions.len(),
    })))
}

// ---------------------------------------------------------------------------
// Mood-based playlist
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MoodRequest {
    mood: String,
    limit: Option<i64>,
}

async fn mood_playlist(
    State(state): State<AppState>,
    Json(body): Json<MoodRequest>,
) -> Result<Json<Value>, AppError> {
    let limit = body.limit.unwrap_or(20);

    let (genres, bpm_min, bpm_max): (&[&str], f64, f64) = match body.mood.as_str() {
        "happy" => (&["pop", "funk", "disco", "soul", "dance"], 110.0, 140.0),
        "sad" => (
            &["blues", "ballad", "acoustic", "folk", "singer-songwriter"],
            50.0,
            90.0,
        ),
        "energetic" => (
            &["rock", "electronic", "dance", "punk", "metal"],
            120.0,
            180.0,
        ),
        "calm" | "relaxed" => (
            &["jazz", "classical", "ambient", "new age", "lounge"],
            50.0,
            90.0,
        ),
        "focus" => (
            &[
                "ambient",
                "classical",
                "electronic",
                "instrumental",
                "post-rock",
            ],
            70.0,
            120.0,
        ),
        "romantic" => (
            &["soul", "r&b", "jazz", "bossa nova", "chanson"],
            60.0,
            110.0,
        ),
        _ => (&["pop", "rock", "jazz", "electronic"], 80.0, 140.0),
    };

    let genre_conditions: Vec<String> = genres
        .iter()
        .map(|g| {
            format!(
                "t.genre LIKE '%{}%' OR t.genres LIKE '%{}%'",
                g.replace('\'', "''"),
                g.replace('\'', "''"),
            )
        })
        .collect();
    let genre_where = if genre_conditions.is_empty() {
        "1=1".to_string()
    } else {
        format!("({})", genre_conditions.join(" OR "))
    };

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE ({genre_where}) AND (t.bpm IS NULL OR t.bpm BETWEEN ? AND ?) \
         ORDER BY RANDOM() \
         LIMIT ?",
    );

    let tracks: Vec<Value> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![bpm_min, bpm_max, limit], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "smart_ai_mood_query_failed");
            vec![]
        });

    // If genre filter returned too few results, fall back to BPM-only
    let tracks = if tracks.len() < 5 {
        let fallback_sql = "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE (t.bpm IS NULL OR t.bpm BETWEEN ? AND ?) \
             ORDER BY RANDOM() \
             LIMIT ?";
        state
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(fallback_sql)?;
                let rows = stmt.query_map(rusqlite::params![bpm_min, bpm_max, limit], |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default()
    } else {
        tracks
    };

    Ok(Json(json!({
        "name": format!("{} Mix", capitalize(&body.mood)),
        "mood": body.mood,
        "tracks": tracks,
        "total": tracks.len(),
        "bpm_range": [bpm_min, bpm_max],
        "target_genres": genres,
    })))
}

// ---------------------------------------------------------------------------
// Similar-to playlist
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SimilarToRequest {
    track_id: Option<i64>,
    album_id: Option<i64>,
    limit: Option<i64>,
}

async fn similar_to_playlist(
    State(state): State<AppState>,
    Json(body): Json<SimilarToRequest>,
) -> Result<Json<Value>, AppError> {
    let limit = body.limit.unwrap_or(20);

    // Get the reference track/album attributes
    let (genre, artist_id, year, bpm): (Option<String>, Option<i64>, Option<i32>, Option<f64>) =
        if let Some(track_id) = body.track_id {
            state
                .db
                .read(|conn| {
                    conn.query_row(
                        "SELECT genre, artist_id, year, bpm FROM tracks WHERE id = ?",
                        rusqlite::params![track_id],
                        |row| {
                            Ok((
                                row.get(0).ok().flatten(),
                                row.get(1).ok().flatten(),
                                row.get(2).ok().flatten(),
                                row.get(3).ok().flatten(),
                            ))
                        },
                    )
                })
                .unwrap_or((None, None, None, None))
        } else if let Some(album_id) = body.album_id {
            state
                .db
                .read(|conn| {
                    conn.query_row(
                        "SELECT al.genre, al.artist_id, al.year, NULL FROM albums al WHERE al.id = ?",
                        rusqlite::params![album_id],
                        |row| {
                            Ok((
                                row.get(0).ok().flatten(),
                                row.get(1).ok().flatten(),
                                row.get(2).ok().flatten(),
                                None,
                            ))
                        },
                    )
                })
                .unwrap_or((None, None, None, None))
        } else {
            return Err(AppError::bad_request("provide track_id or album_id"));
        };

    // Build similarity conditions with scoring
    let mut conditions = Vec::new();
    let mut score_parts = Vec::new();

    if let Some(ref g) = genre {
        let escaped = g.replace('\'', "''");
        conditions.push(format!(
            "(t.genre LIKE '%{escaped}%' OR t.genres LIKE '%{escaped}%')"
        ));
        score_parts.push(format!(
            "CASE WHEN t.genre LIKE '%{escaped}%' THEN 3 ELSE 0 END"
        ));
    }
    if let Some(aid) = artist_id {
        score_parts.push(format!("CASE WHEN t.artist_id = {aid} THEN 5 ELSE 0 END"));
    }
    if let Some(y) = year {
        score_parts.push(format!(
            "CASE WHEN t.year BETWEEN {} AND {} THEN 2 ELSE 0 END",
            y - 5,
            y + 5
        ));
    }
    if let Some(b) = bpm {
        score_parts.push(format!(
            "CASE WHEN t.bpm BETWEEN {} AND {} THEN 2 ELSE 0 END",
            b - 15.0,
            b + 15.0
        ));
    }

    let score_expr = if score_parts.is_empty() {
        "0".to_string()
    } else {
        format!("({})", score_parts.join(" + "))
    };

    // Exclude the source track/album
    let exclude = if let Some(tid) = body.track_id {
        format!("t.id != {tid}")
    } else if let Some(aid) = body.album_id {
        format!("t.album_id != {aid}")
    } else {
        "1=1".into()
    };

    let where_clause = if conditions.is_empty() {
        exclude
    } else {
        format!("{} AND {}", conditions.join(" AND "), exclude)
    };

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path, \
         {score_expr} as similarity_score \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE {where_clause} \
         ORDER BY similarity_score DESC, RANDOM() \
         LIMIT ?",
    );

    let tracks: Vec<Value> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                    "similarity_score": row.get::<_, Option<i64>>(9).ok().flatten(),
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Ok(Json(json!({
        "name": "Similar Tracks",
        "reference": {
            "track_id": body.track_id,
            "album_id": body.album_id,
            "genre": genre,
            "year": year,
            "bpm": bpm,
        },
        "tracks": tracks,
        "total": tracks.len(),
    })))
}

// ---------------------------------------------------------------------------
// History-based "Your Mix"
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryBasedRequest {
    limit: Option<i64>,
    days: Option<i64>,
}

async fn history_based_playlist(
    State(state): State<AppState>,
    Json(body): Json<HistoryBasedRequest>,
) -> Result<Json<Value>, AppError> {
    let limit = body.limit.unwrap_or(30);
    let days = body.days.unwrap_or(30);

    // Get top genres from listening history
    let top_genres: Vec<String> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.genre, COUNT(*) as cnt \
                 FROM listen_history lh \
                 JOIN tracks t ON lh.track_id = t.id \
                 WHERE t.genre IS NOT NULL \
                   AND lh.listened_at >= datetime('now', '-' || ? || ' days') \
                 GROUP BY t.genre \
                 ORDER BY cnt DESC \
                 LIMIT 5",
            )?;
            let rows = stmt.query_map(rusqlite::params![days], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    // Get top artists from history
    let top_artist_ids: Vec<i64> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.artist_id, COUNT(*) as cnt \
                 FROM listen_history lh \
                 JOIN tracks t ON lh.track_id = t.id \
                 WHERE t.artist_id IS NOT NULL \
                   AND lh.listened_at >= datetime('now', '-' || ? || ' days') \
                 GROUP BY t.artist_id \
                 ORDER BY cnt DESC \
                 LIMIT 10",
            )?;
            let rows = stmt.query_map(rusqlite::params![days], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    // Build a mix: 60% from top artists, 40% from top genres (unplayed)
    let artist_limit = (limit * 60 / 100).max(1);
    let genre_limit = limit - artist_limit;

    let mut all_tracks = Vec::new();

    if !top_artist_ids.is_empty() {
        let placeholders: Vec<String> = top_artist_ids.iter().map(|id| id.to_string()).collect();
        let in_clause = placeholders.join(",");
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.artist_id IN ({in_clause}) \
             ORDER BY RANDOM() \
             LIMIT ?",
        );
        let artist_tracks: Vec<Value> = state
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![artist_limit], |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                        "source_reason": "top_artist",
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        all_tracks.extend(artist_tracks);
    }

    if !top_genres.is_empty() {
        let genre_conditions: Vec<String> = top_genres
            .iter()
            .map(|g| format!("t.genre LIKE '%{}%'", g.replace('\'', "''")))
            .collect();
        let genre_where = genre_conditions.join(" OR ");

        // Get unplayed tracks from those genres
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE ({genre_where}) \
               AND t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
             ORDER BY RANDOM() \
             LIMIT ?",
        );
        let genre_tracks: Vec<Value> = state
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![genre_limit], |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                        "source_reason": "genre_discovery",
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        all_tracks.extend(genre_tracks);
    }

    // If history is empty, fall back to random selection
    if all_tracks.is_empty() {
        all_tracks = state
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
                     FROM tracks t \
                     LEFT JOIN albums al ON t.album_id = al.id \
                     LEFT JOIN artists ar ON t.artist_id = ar.id \
                     ORDER BY RANDOM() \
                     LIMIT ?",
                )?;
                let rows = stmt.query_map(rusqlite::params![limit], |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                        "source_reason": "random_fallback",
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
    }

    Ok(Json(json!({
        "name": "Your Mix",
        "tracks": all_tracks,
        "total": all_tracks.len(),
        "top_genres": top_genres,
        "top_artist_count": top_artist_ids.len(),
        "days_analyzed": days,
    })))
}

// ---------------------------------------------------------------------------
// Tempo-matching playlist
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TempoMatchRequest {
    target_bpm: f64,
    tolerance: Option<f64>,
    limit: Option<i64>,
}

async fn tempo_match_playlist(
    State(state): State<AppState>,
    Json(body): Json<TempoMatchRequest>,
) -> Result<Json<Value>, AppError> {
    let tolerance = body.tolerance.unwrap_or(10.0);
    let limit = body.limit.unwrap_or(20);
    let bpm_min = body.target_bpm - tolerance;
    let bpm_max = body.target_bpm + tolerance;

    let tracks: Vec<Value> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
                 FROM tracks t \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 WHERE t.bpm IS NOT NULL AND t.bpm BETWEEN ? AND ? \
                 ORDER BY ABS(t.bpm - ?) ASC \
                 LIMIT ?",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![bpm_min, bpm_max, body.target_bpm, limit],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                    }))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Ok(Json(json!({
        "name": format!("{}BPM Mix", body.target_bpm as i32),
        "target_bpm": body.target_bpm,
        "tolerance": tolerance,
        "bpm_range": [bpm_min, bpm_max],
        "tracks": tracks,
        "total": tracks.len(),
    })))
}

// ---------------------------------------------------------------------------
// Discovery playlist: unplayed tracks from genres you like
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DiscoveryRequest {
    limit: Option<i64>,
}

async fn discovery_playlist(
    State(state): State<AppState>,
    Json(body): Json<DiscoveryRequest>,
) -> Result<Json<Value>, AppError> {
    let limit = body.limit.unwrap_or(30);

    // Get top genres from full history
    let top_genres: Vec<String> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT t.genre, COUNT(*) as cnt \
                 FROM listen_history lh \
                 JOIN tracks t ON lh.track_id = t.id \
                 WHERE t.genre IS NOT NULL \
                 GROUP BY t.genre \
                 ORDER BY cnt DESC \
                 LIMIT 5",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    if top_genres.is_empty() {
        // No history: return random unplayed tracks
        let tracks: Vec<Value> = state
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
                     FROM tracks t \
                     LEFT JOIN albums al ON t.album_id = al.id \
                     LEFT JOIN artists ar ON t.artist_id = ar.id \
                     WHERE t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
                     ORDER BY RANDOM() \
                     LIMIT ?",
                )?;
                let rows = stmt.query_map(rusqlite::params![limit], |row| {
                    Ok(json!({
                        "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                        "title": row.get::<_, Option<String>>(1).ok().flatten(),
                        "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                        "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                        "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                        "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                        "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                        "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                        "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                    }))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();

        return Ok(Json(json!({
            "name": "Discovery Mix",
            "tracks": tracks,
            "total": tracks.len(),
            "top_genres": [],
            "message": "No listening history yet - showing random unplayed tracks",
        })));
    }

    // Find unplayed tracks from favorite genres
    let genre_conditions: Vec<String> = top_genres
        .iter()
        .map(|g| format!("t.genre LIKE '%{}%'", g.replace('\'', "''")))
        .collect();
    let genre_where = genre_conditions.join(" OR ");

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm, al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE ({genre_where}) \
           AND t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
         ORDER BY RANDOM() \
         LIMIT ?",
    );

    let tracks: Vec<Value> = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "title": row.get::<_, Option<String>>(1).ok().flatten(),
                    "artist_name": row.get::<_, Option<String>>(2).ok().flatten(),
                    "album_title": row.get::<_, Option<String>>(3).ok().flatten(),
                    "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
                    "genre": row.get::<_, Option<String>>(5).ok().flatten(),
                    "year": row.get::<_, Option<i32>>(6).ok().flatten(),
                    "bpm": row.get::<_, Option<f64>>(7).ok().flatten(),
                    "cover_path": row.get::<_, Option<String>>(8).ok().flatten(),
                }))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();

    Ok(Json(json!({
        "name": "Discovery Mix",
        "tracks": tracks,
        "total": tracks.len(),
        "top_genres": top_genres,
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}
