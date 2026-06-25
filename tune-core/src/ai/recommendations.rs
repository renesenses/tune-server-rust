//! AI-powered music recommendations.
//!
//! Content-based filtering using genre matching + artist co-occurrence
//! from listen_history. No ML — just smart SQL queries.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendedTrack {
    pub track_id: i64,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration_ms: i64,
    pub cover_path: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyMix {
    pub name: String,
    pub description: String,
    pub tracks: Vec<RecommendedTrack>,
}

// ---------------------------------------------------------------------------
// Row → struct helpers
// ---------------------------------------------------------------------------

fn row_to_track(row: &[SqlValue], reason: &str) -> RecommendedTrack {
    RecommendedTrack {
        track_id: row.first().and_then(|v| v.as_i64()).unwrap_or(0),
        title: row.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        artist: row.get(2).and_then(|v| v.as_string()),
        album: row.get(3).and_then(|v| v.as_string()),
        genre: row.get(4).and_then(|v| v.as_string()),
        duration_ms: row.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
        cover_path: row.get(6).and_then(|v| v.as_string()),
        reason: reason.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Track columns selected by all queries
// ---------------------------------------------------------------------------

const TRACK_COLS: &str = "\
    t.id, t.title, \
    COALESCE(a.name, t.album_artist) as artist, \
    al.title as album_title, \
    t.genre, t.duration_ms, al.cover_path";

// ---------------------------------------------------------------------------
// get_recommendations
// ---------------------------------------------------------------------------

/// Personalized recommendations based on listening history.
///
/// Algorithm:
/// 1. Find top 5 genres from listen_history (joined with tracks table)
/// 2. Find top 5 artists from listen_history
/// 3. Select tracks matching those genres/artists that haven't been
///    played in the last 7 days
/// 4. Order by RANDOM() to keep it fresh
pub fn get_recommendations(
    backend: &Arc<dyn DbBackend>,
    _seed_track_id: Option<i64>,
    limit: i64,
) -> Vec<RecommendedTrack> {
    let mut results = Vec::new();

    // --- Top genres from history (join tracks to get genre) ---
    let top_genres = backend
        .query_many(
            "SELECT t.genre, COUNT(*) as c \
             FROM listen_history h \
             JOIN tracks t ON CAST(h.track_id AS INTEGER) = t.id \
             WHERE t.genre IS NOT NULL AND t.genre != '' \
             GROUP BY t.genre ORDER BY c DESC LIMIT 5",
            &[],
        )
        .unwrap_or_default();

    let genre_names: Vec<String> = top_genres
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect();

    debug!(genres = ?genre_names, "ai_top_genres");

    // --- Top artists from history ---
    let top_artists = backend
        .query_many(
            "SELECT artist_name, COUNT(*) as c \
             FROM listen_history \
             WHERE artist_name IS NOT NULL AND artist_name != '' \
             AND source != 'radio' \
             GROUP BY artist_name ORDER BY c DESC LIMIT 5",
            &[],
        )
        .unwrap_or_default();

    let artist_names: Vec<String> = top_artists
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect();

    debug!(artists = ?artist_names, "ai_top_artists");

    // --- Tracks matching top genres, not recently played ---
    if !genre_names.is_empty() {
        let placeholders: Vec<String> = genre_names.iter().map(|_| "?".to_string()).collect();
        let in_clause = placeholders.join(", ");
        let half_limit = (limit / 2).max(5);

        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             WHERE t.genre IN ({in_clause}) \
             AND t.id NOT IN ( \
                 SELECT CAST(track_id AS INTEGER) FROM listen_history \
                 WHERE listened_at > datetime('now', '-7 days') \
             ) \
             ORDER BY RANDOM() LIMIT ?"
        );

        let mut params: Vec<Box<dyn ToSqlValue>> = genre_names
            .iter()
            .map(|g| Box::new(g.clone()) as Box<dyn ToSqlValue>)
            .collect();
        params.push(Box::new(half_limit));

        let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();

        if let Ok(rows) = backend.query_many(&sql, &param_refs) {
            for row in &rows {
                results.push(row_to_track(row, "genre match"));
            }
        }
    }

    // --- Tracks from top artists, not recently played ---
    if !artist_names.is_empty() {
        let remaining = (limit as usize).saturating_sub(results.len());
        if remaining > 0 {
            let placeholders: Vec<String> = artist_names.iter().map(|_| "?".to_string()).collect();
            let in_clause = placeholders.join(", ");

            let sql = format!(
                "SELECT {TRACK_COLS} \
                 FROM tracks t \
                 LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
                 LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
                 WHERE COALESCE(a.name, t.album_artist) IN ({in_clause}) \
                 AND t.id NOT IN ( \
                     SELECT CAST(track_id AS INTEGER) FROM listen_history \
                     WHERE listened_at > datetime('now', '-7 days') \
                 ) \
                 AND t.id NOT IN ({}) \
                 ORDER BY RANDOM() LIMIT ?",
                if results.is_empty() {
                    "0".to_string()
                } else {
                    results
                        .iter()
                        .map(|r| r.track_id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );

            let mut params: Vec<Box<dyn ToSqlValue>> = artist_names
                .iter()
                .map(|a| Box::new(a.clone()) as Box<dyn ToSqlValue>)
                .collect();
            params.push(Box::new(remaining as i64));

            let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();

            if let Ok(rows) = backend.query_many(&sql, &param_refs) {
                for row in &rows {
                    results.push(row_to_track(row, "artist affinity"));
                }
            }
        }
    }

    // --- Fallback: random tracks if history is empty ---
    if results.is_empty() {
        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             ORDER BY RANDOM() LIMIT ?"
        );
        let lim = limit;
        if let Ok(rows) = backend.query_many(&sql, &[&lim]) {
            for row in &rows {
                results.push(row_to_track(row, "discovery"));
            }
        }
    }

    info!(count = results.len(), "ai_recommendations_generated");
    results
}

// ---------------------------------------------------------------------------
// generate_daily_mixes
// ---------------------------------------------------------------------------

/// Generate 3-5 thematic daily mixes based on top genres from history.
/// Each mix is a named playlist of ~15 tracks. Stored in settings as JSON
/// so the UI can poll it without re-computing.
pub fn generate_daily_mixes(backend: &Arc<dyn DbBackend>) -> Vec<DailyMix> {
    let mut mixes = Vec::new();

    // Get top genres
    let top_genres = backend
        .query_many(
            "SELECT t.genre, COUNT(*) as c \
             FROM listen_history h \
             JOIN tracks t ON CAST(h.track_id AS INTEGER) = t.id \
             WHERE t.genre IS NOT NULL AND t.genre != '' \
             GROUP BY t.genre ORDER BY c DESC LIMIT 5",
            &[],
        )
        .unwrap_or_default();

    let genre_names: Vec<String> = top_genres
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect();

    // --- Mix per top genre ---
    for genre in &genre_names {
        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             WHERE t.genre = ? \
             ORDER BY RANDOM() LIMIT 15"
        );

        if let Ok(rows) = backend.query_many(&sql, &[genre as &dyn ToSqlValue]) {
            if rows.len() >= 3 {
                let tracks: Vec<RecommendedTrack> = rows
                    .iter()
                    .map(|r| row_to_track(r, &format!("{genre} mix")))
                    .collect();
                mixes.push(DailyMix {
                    name: format!("{genre} Mix"),
                    description: format!("Your favorites in {genre}"),
                    tracks,
                });
            }
        }

        if mixes.len() >= 5 {
            break;
        }
    }

    // --- "Rediscover" mix: tracks played >30 days ago ---
    if mixes.len() < 5 {
        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             WHERE t.id IN ( \
                 SELECT CAST(track_id AS INTEGER) FROM listen_history \
                 WHERE listened_at < datetime('now', '-30 days') \
                 AND listened_at > datetime('now', '-180 days') \
             ) \
             AND t.id NOT IN ( \
                 SELECT CAST(track_id AS INTEGER) FROM listen_history \
                 WHERE listened_at > datetime('now', '-7 days') \
             ) \
             ORDER BY RANDOM() LIMIT 15"
        );

        if let Ok(rows) = backend.query_many(&sql, &[]) {
            if rows.len() >= 3 {
                let tracks: Vec<RecommendedTrack> =
                    rows.iter().map(|r| row_to_track(r, "rediscover")).collect();
                mixes.push(DailyMix {
                    name: "Rediscover".to_string(),
                    description: "Tracks you haven't listened to in a while".to_string(),
                    tracks,
                });
            }
        }
    }

    // Store in settings for quick retrieval
    if let Ok(json_str) = serde_json::to_string(&mixes) {
        let settings = SettingsRepo::with_backend(backend.clone());
        let _ = settings.set("ai_daily_mixes", &json_str);
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let _ = settings.set("ai_daily_mixes_updated_at", &now);
    }

    info!(count = mixes.len(), "ai_daily_mixes_generated");
    mixes
}

// ---------------------------------------------------------------------------
// smart_radio
// ---------------------------------------------------------------------------

/// Smart radio: find tracks similar to a seed track, artist, or genre.
///
/// Algorithm:
/// 1. Look up the seed track's genre + artist
/// 2. Find tracks with the same genre (weighted)
/// 3. Find tracks from artists that co-occur in listening sessions
/// 4. Mix and return up to `count` tracks
pub fn smart_radio(
    backend: &Arc<dyn DbBackend>,
    seed_track_id: Option<i64>,
    seed_artist: Option<&str>,
    seed_genre: Option<&str>,
    count: usize,
) -> Vec<RecommendedTrack> {
    let mut results = Vec::new();
    let count_i64 = count as i64;

    // Resolve seed metadata
    let (genre, artist) = if let Some(tid) = seed_track_id {
        let row = backend
            .query_one(
                "SELECT t.genre, COALESCE(a.name, t.album_artist) \
                 FROM tracks t \
                 LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
                 WHERE t.id = ?",
                &[&tid],
            )
            .ok()
            .flatten();
        match row {
            Some(r) => (
                r.first().and_then(|v| v.as_string()),
                r.get(1).and_then(|v| v.as_string()),
            ),
            None => (None, None),
        }
    } else {
        (
            seed_genre.map(|s| s.to_string()),
            seed_artist.map(|s| s.to_string()),
        )
    };

    debug!(seed_genre = ?genre, seed_artist = ?artist, "smart_radio_seed");

    // --- Same genre tracks ---
    if let Some(ref g) = genre {
        let half = (count_i64 / 2).max(5);
        let exclude_id = seed_track_id.unwrap_or(0);
        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             WHERE t.genre = ? AND t.id != ? \
             ORDER BY RANDOM() LIMIT ?"
        );

        if let Ok(rows) = backend.query_many(&sql, &[g as &dyn ToSqlValue, &exclude_id, &half]) {
            for row in &rows {
                results.push(row_to_track(row, "same genre"));
            }
        }
    }

    // --- Co-occurring artists: artists listened in same sessions ---
    if let Some(ref art) = artist {
        let remaining = count.saturating_sub(results.len());
        if remaining > 0 {
            // Find artists that appear in the same listening sessions (same day)
            let co_artists = backend
                .query_many(
                    "SELECT h2.artist_name, COUNT(*) as c \
                     FROM listen_history h1 \
                     JOIN listen_history h2 ON date(h1.listened_at) = date(h2.listened_at) \
                     WHERE h1.artist_name = ? \
                     AND h2.artist_name != ? \
                     AND h2.artist_name IS NOT NULL \
                     GROUP BY h2.artist_name \
                     ORDER BY c DESC LIMIT 5",
                    &[art as &dyn ToSqlValue, art as &dyn ToSqlValue],
                )
                .unwrap_or_default();

            let co_artist_names: Vec<String> = co_artists
                .iter()
                .filter_map(|r| r.first().and_then(|v| v.as_string()))
                .collect();

            if !co_artist_names.is_empty() {
                let placeholders: Vec<String> =
                    co_artist_names.iter().map(|_| "?".to_string()).collect();
                let in_clause = placeholders.join(", ");
                let exclude_ids = if results.is_empty() {
                    "0".to_string()
                } else {
                    results
                        .iter()
                        .map(|r| r.track_id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let remaining_i64 = remaining as i64;

                let sql = format!(
                    "SELECT {TRACK_COLS} \
                     FROM tracks t \
                     LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
                     LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
                     WHERE COALESCE(a.name, t.album_artist) IN ({in_clause}) \
                     AND t.id NOT IN ({exclude_ids}) \
                     ORDER BY RANDOM() LIMIT ?"
                );

                let mut params: Vec<Box<dyn ToSqlValue>> = co_artist_names
                    .iter()
                    .map(|a| Box::new(a.clone()) as Box<dyn ToSqlValue>)
                    .collect();
                params.push(Box::new(remaining_i64));

                let param_refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p.as_ref()).collect();

                if let Ok(rows) = backend.query_many(&sql, &param_refs) {
                    for row in &rows {
                        results.push(row_to_track(row, "artist co-occurrence"));
                    }
                }
            }
        }
    }

    // --- Same artist tracks (fill remaining) ---
    if let Some(ref art) = artist {
        let remaining = count.saturating_sub(results.len());
        if remaining > 0 {
            let exclude_id = seed_track_id.unwrap_or(0);
            let exclude_ids = if results.is_empty() {
                "0".to_string()
            } else {
                results
                    .iter()
                    .map(|r| r.track_id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let remaining_i64 = remaining as i64;

            let sql = format!(
                "SELECT {TRACK_COLS} \
                 FROM tracks t \
                 LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
                 LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
                 WHERE COALESCE(a.name, t.album_artist) = ? \
                 AND t.id != ? \
                 AND t.id NOT IN ({exclude_ids}) \
                 ORDER BY RANDOM() LIMIT ?"
            );

            if let Ok(rows) =
                backend.query_many(&sql, &[art as &dyn ToSqlValue, &exclude_id, &remaining_i64])
            {
                for row in &rows {
                    results.push(row_to_track(row, "same artist"));
                }
            }
        }
    }

    // --- Fallback: random tracks ---
    let remaining = count.saturating_sub(results.len());
    if remaining > 0 {
        let exclude_ids = if results.is_empty() {
            "0".to_string()
        } else {
            results
                .iter()
                .map(|r| r.track_id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let remaining_i64 = remaining as i64;

        let sql = format!(
            "SELECT {TRACK_COLS} \
             FROM tracks t \
             LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
             LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
             WHERE t.id NOT IN ({exclude_ids}) \
             ORDER BY RANDOM() LIMIT ?"
        );

        if let Ok(rows) = backend.query_many(&sql, &[&remaining_i64]) {
            for row in &rows {
                results.push(row_to_track(row, "discovery"));
            }
        }
    }

    info!(count = results.len(), "smart_radio_generated");
    results
}

// ---------------------------------------------------------------------------
// Cached daily mixes retrieval
// ---------------------------------------------------------------------------

/// Load daily mixes from settings cache. Returns None if not generated yet
/// or if the cache is older than 24 hours.
pub fn get_cached_daily_mixes(backend: &Arc<dyn DbBackend>) -> Option<Vec<DailyMix>> {
    let settings = SettingsRepo::with_backend(backend.clone());

    let updated_at = settings.get("ai_daily_mixes_updated_at").ok()??;
    // Check if cache is fresh (less than 24h old)
    if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(&updated_at, "%Y-%m-%dT%H:%M:%SZ") {
        let validated = parsed.and_utc();
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
        if validated < cutoff {
            return None;
        }
    }

    let json_str = settings.get("ai_daily_mixes").ok()??;
    serde_json::from_str(&json_str).ok()
}
