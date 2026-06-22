use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

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

/// Returns a positional placeholder for the engine.
fn ph(engine: Engine, idx: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(idx),
        Engine::Postgres => PostgresDialect.placeholder(idx),
    }
}

/// Build a `listened_at >= now - N days` fragment for the engine.
/// `days` is embedded numerically in the SQL (safe: it's an i64 from the user body).
fn since_days_sql(engine: Engine, column: &str, days: i64) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.since_days(column, days),
        Engine::Postgres => PostgresDialect.since_days(column, days),
    }
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
    let engine = state.backend.engine();

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
    let bpm_cast = if engine == Engine::Postgres {
        "CAST(t.bpm AS float8)"
    } else {
        "t.bpm"
    };

    if prompt.contains("relax") || prompt.contains("calm") || prompt.contains("chill") {
        conditions.push(format!(
            "({bpm_cast} IS NULL OR {bpm_cast} BETWEEN 50 AND 100)"
        ));
        order_by = "t.bpm ASC NULLS LAST";
    }
    if prompt.contains("evening") || prompt.contains("night") || prompt.contains("soir") {
        conditions.push(format!(
            "({bpm_cast} IS NULL OR {bpm_cast} BETWEEN 50 AND 110)"
        ));
    }
    if prompt.contains("morning") || prompt.contains("matin") {
        conditions.push(format!(
            "({bpm_cast} IS NULL OR {bpm_cast} BETWEEN 80 AND 130)"
        ));
    }
    if prompt.contains("energi") || prompt.contains("workout") || prompt.contains("sport") {
        conditions.push(format!(
            "({bpm_cast} IS NULL OR {bpm_cast} BETWEEN 120 AND 180)"
        ));
        order_by = "t.bpm DESC NULLS LAST";
    }
    if prompt.contains("focus") || prompt.contains("study") || prompt.contains("concentr") {
        conditions.push(format!(
            "({bpm_cast} IS NULL OR {bpm_cast} BETWEEN 70 AND 120)"
        ));
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

    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
         t.genre, t.year, CAST(t.bpm AS float8), al.cover_path, t.format \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         {where_clause} \
         ORDER BY {order_by} \
         LIMIT {p1}",
    );

    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                "bpm": cols.get(7).and_then(|v| v.as_f64()),
                "cover_path": cols.get(8).and_then(|v| v.as_string()),
                "format": cols.get(9).and_then(|v| v.as_string()),
            })
        })
        .collect();

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
    let engine = state.backend.engine();

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

    let bpm_cast = if engine == Engine::Postgres {
        "CAST(t.bpm AS float8)"
    } else {
        "t.bpm"
    };
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let p3 = ph(engine, 3);

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
         t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE ({genre_where}) AND ({bpm_cast} IS NULL OR {bpm_cast} BETWEEN {p1} AND {p2}) \
         ORDER BY RANDOM() \
         LIMIT {p3}",
    );

    let rows = state
        .backend
        .query_many(
            &sql,
            &[
                &bpm_min as &dyn ToSqlValue,
                &bpm_max as &dyn ToSqlValue,
                &limit as &dyn ToSqlValue,
            ],
        )
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "smart_ai_mood_query_failed");
            vec![]
        });

    let decode_track_row = |cols: &Vec<tune_core::db::backend::SqlValue>| {
        json!({
            "id": cols.get(0).and_then(|v| v.as_i64()),
            "title": cols.get(1).and_then(|v| v.as_string()),
            "artist_name": cols.get(2).and_then(|v| v.as_string()),
            "album_title": cols.get(3).and_then(|v| v.as_string()),
            "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
            "genre": cols.get(5).and_then(|v| v.as_string()),
            "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
            "bpm": cols.get(7).and_then(|v| v.as_f64()),
            "cover_path": cols.get(8).and_then(|v| v.as_string()),
        })
    };

    let tracks: Vec<Value> = rows.iter().map(decode_track_row).collect();

    // If genre filter returned too few results, fall back to BPM-only
    let tracks = if tracks.len() < 5 {
        let fallback_sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
             t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE ({bpm_cast} IS NULL OR {bpm_cast} BETWEEN {p1} AND {p2}) \
             ORDER BY RANDOM() \
             LIMIT {p3}",
        );
        state
            .backend
            .query_many(
                &fallback_sql,
                &[
                    &bpm_min as &dyn ToSqlValue,
                    &bpm_max as &dyn ToSqlValue,
                    &limit as &dyn ToSqlValue,
                ],
            )
            .unwrap_or_default()
            .iter()
            .map(decode_track_row)
            .collect()
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
    let engine = state.backend.engine();

    // Get the reference track/album attributes
    let (genre, artist_id, year, bpm): (Option<String>, Option<i64>, Option<i64>, Option<f64>) =
        if let Some(track_id) = body.track_id {
            let p1 = ph(engine, 1);
            let sql = format!(
                "SELECT t.genre, t.artist_id, t.year, CAST(t.bpm AS float8) FROM tracks t WHERE t.id = {p1}"
            );
            state
                .backend
                .query_one(&sql, &[&track_id as &dyn ToSqlValue])
                .unwrap_or(None)
                .map(|cols| {
                    (
                        cols.get(0).and_then(|v| v.as_string()),
                        cols.get(1).and_then(|v| v.as_i64()),
                        cols.get(2).and_then(|v| v.as_i64()),
                        cols.get(3).and_then(|v| v.as_f64()),
                    )
                })
                .unwrap_or((None, None, None, None))
        } else if let Some(album_id) = body.album_id {
            let p1 = ph(engine, 1);
            let sql = format!(
                "SELECT al.genre, al.artist_id, al.year, NULL FROM albums al WHERE al.id = {p1}"
            );
            state
                .backend
                .query_one(&sql, &[&album_id as &dyn ToSqlValue])
                .unwrap_or(None)
                .map(|cols| {
                    (
                        cols.get(0).and_then(|v| v.as_string()),
                        cols.get(1).and_then(|v| v.as_i64()),
                        cols.get(2).and_then(|v| v.as_i64()),
                        None,
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
        let bpm_cast = if engine == Engine::Postgres {
            "CAST(t.bpm AS float8)"
        } else {
            "t.bpm"
        };
        score_parts.push(format!(
            "CASE WHEN {bpm_cast} BETWEEN {} AND {} THEN 2 ELSE 0 END",
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

    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
         t.genre, t.year, CAST(t.bpm AS float8), al.cover_path, \
         {score_expr} as similarity_score \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE {where_clause} \
         ORDER BY similarity_score DESC, RANDOM() \
         LIMIT {p1}",
    );

    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                "bpm": cols.get(7).and_then(|v| v.as_f64()),
                "cover_path": cols.get(8).and_then(|v| v.as_string()),
                "similarity_score": cols.get(9).and_then(|v| v.as_i64()),
            })
        })
        .collect();

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
    let engine = state.backend.engine();

    // Build the date filter fragment (days is embedded numerically)
    let date_filter = since_days_sql(engine, "lh.listened_at", days);

    // Get top genres from listening history
    let top_genres_sql = format!(
        "SELECT t.genre, COUNT(*) as cnt \
         FROM listen_history lh \
         JOIN tracks t ON lh.track_id = t.id \
         WHERE t.genre IS NOT NULL \
           AND {date_filter} \
         GROUP BY t.genre \
         ORDER BY cnt DESC \
         LIMIT 5"
    );
    let top_genres: Vec<String> = state
        .backend
        .query_many(&top_genres_sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_string()))
        .collect();

    // Get top artists from history
    let top_artists_sql = format!(
        "SELECT t.artist_id, COUNT(*) as cnt \
         FROM listen_history lh \
         JOIN tracks t ON lh.track_id = t.id \
         WHERE t.artist_id IS NOT NULL \
           AND {date_filter} \
         GROUP BY t.artist_id \
         ORDER BY cnt DESC \
         LIMIT 10"
    );
    let top_artist_ids: Vec<i64> = state
        .backend
        .query_many(&top_artists_sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_i64()))
        .collect();

    // Build a mix: 60% from top artists, 40% from top genres (unplayed)
    let artist_limit = (limit * 60 / 100).max(1);
    let genre_limit = limit - artist_limit;

    let mut all_tracks = Vec::new();

    if !top_artist_ids.is_empty() {
        let placeholders: Vec<String> = top_artist_ids.iter().map(|id| id.to_string()).collect();
        let in_clause = placeholders.join(",");
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
             t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.artist_id IN ({in_clause}) \
             ORDER BY RANDOM() \
             LIMIT {p1}",
        );
        let artist_tracks: Vec<Value> = state
            .backend
            .query_many(&sql, &[&artist_limit as &dyn ToSqlValue])
            .unwrap_or_default()
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                    "bpm": cols.get(7).and_then(|v| v.as_f64()),
                    "cover_path": cols.get(8).and_then(|v| v.as_string()),
                    "source_reason": "top_artist",
                })
            })
            .collect();
        all_tracks.extend(artist_tracks);
    }

    if !top_genres.is_empty() {
        let genre_conditions: Vec<String> = top_genres
            .iter()
            .map(|g| format!("t.genre LIKE '%{}%'", g.replace('\'', "''")))
            .collect();
        let genre_where = genre_conditions.join(" OR ");

        let p1 = ph(engine, 1);
        // Get unplayed tracks from those genres
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
             t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE ({genre_where}) \
               AND t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
             ORDER BY RANDOM() \
             LIMIT {p1}",
        );
        let genre_tracks: Vec<Value> = state
            .backend
            .query_many(&sql, &[&genre_limit as &dyn ToSqlValue])
            .unwrap_or_default()
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                    "bpm": cols.get(7).and_then(|v| v.as_f64()),
                    "cover_path": cols.get(8).and_then(|v| v.as_string()),
                    "source_reason": "genre_discovery",
                })
            })
            .collect();
        all_tracks.extend(genre_tracks);
    }

    // If history is empty, fall back to random selection
    if all_tracks.is_empty() {
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
             t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             ORDER BY RANDOM() \
             LIMIT {p1}",
        );
        all_tracks = state
            .backend
            .query_many(&sql, &[&limit as &dyn ToSqlValue])
            .unwrap_or_default()
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                    "bpm": cols.get(7).and_then(|v| v.as_f64()),
                    "cover_path": cols.get(8).and_then(|v| v.as_string()),
                    "source_reason": "random_fallback",
                })
            })
            .collect();
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
    let engine = state.backend.engine();

    let bpm_cast = if engine == Engine::Postgres {
        "CAST(t.bpm AS float8)"
    } else {
        "t.bpm"
    };
    let p1 = ph(engine, 1);
    let p2 = ph(engine, 2);
    let p3 = ph(engine, 3);
    let p4 = ph(engine, 4);

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
         t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE t.bpm IS NOT NULL AND {bpm_cast} BETWEEN {p1} AND {p2} \
         ORDER BY ABS({bpm_cast} - {p3}) ASC \
         LIMIT {p4}",
    );

    let rows = state
        .backend
        .query_many(
            &sql,
            &[
                &bpm_min as &dyn ToSqlValue,
                &bpm_max as &dyn ToSqlValue,
                &body.target_bpm as &dyn ToSqlValue,
                &limit as &dyn ToSqlValue,
            ],
        )
        .unwrap_or_default();
    let tracks: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                "bpm": cols.get(7).and_then(|v| v.as_f64()),
                "cover_path": cols.get(8).and_then(|v| v.as_string()),
            })
        })
        .collect();

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
    let engine = state.backend.engine();

    // Get top genres from full history
    let top_genres: Vec<String> = state
        .backend
        .query_many(
            "SELECT t.genre, COUNT(*) as cnt \
             FROM listen_history lh \
             JOIN tracks t ON lh.track_id = t.id \
             WHERE t.genre IS NOT NULL \
             GROUP BY t.genre \
             ORDER BY cnt DESC \
             LIMIT 5",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|cols| cols.into_iter().next().and_then(|v| v.as_string()))
        .collect();

    if top_genres.is_empty() {
        // No history: return random unplayed tracks
        let p1 = ph(engine, 1);
        let sql = format!(
            "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
             t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
             FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
             ORDER BY RANDOM() \
             LIMIT {p1}",
        );
        let tracks: Vec<Value> = state
            .backend
            .query_many(&sql, &[&limit as &dyn ToSqlValue])
            .unwrap_or_default()
            .iter()
            .map(|cols| {
                json!({
                    "id": cols.get(0).and_then(|v| v.as_i64()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                    "genre": cols.get(5).and_then(|v| v.as_string()),
                    "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                    "bpm": cols.get(7).and_then(|v| v.as_f64()),
                    "cover_path": cols.get(8).and_then(|v| v.as_string()),
                })
            })
            .collect();

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

    let p1 = ph(engine, 1);
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, CAST(t.duration_ms AS BIGINT), \
         t.genre, t.year, CAST(t.bpm AS float8), al.cover_path \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE ({genre_where}) \
           AND t.id NOT IN (SELECT DISTINCT track_id FROM listen_history WHERE track_id IS NOT NULL) \
         ORDER BY RANDOM() \
         LIMIT {p1}",
    );

    let tracks: Vec<Value> = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .unwrap_or_default()
        .iter()
        .map(|cols| {
            json!({
                "id": cols.get(0).and_then(|v| v.as_i64()),
                "title": cols.get(1).and_then(|v| v.as_string()),
                "artist_name": cols.get(2).and_then(|v| v.as_string()),
                "album_title": cols.get(3).and_then(|v| v.as_string()),
                "duration_ms": cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
                "genre": cols.get(5).and_then(|v| v.as_string()),
                "year": cols.get(6).and_then(|v| v.as_i64()).map(|y| y as i32),
                "bpm": cols.get(7).and_then(|v| v.as_f64()),
                "cover_path": cols.get(8).and_then(|v| v.as_string()),
            })
        })
        .collect();

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
