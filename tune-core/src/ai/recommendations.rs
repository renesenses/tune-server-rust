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

    // --- Acoustic neighbours: tracks that SOUND like the seed, ranked by CLAP
    // cosine. This is the strongest continuity signal, so it goes first. Empty
    // when the seed has no embedding (un-analysed library), leaving the metadata
    // paths below as the fallback — zero regression.
    //
    // Le cosinus seul ne suffit pas (#1820) : dans l'espace CLAP courant, les
    // dix premiers voisins tiennent dans une bande de ~0,09 et dix-sept
    // candidats se pressent à moins de 0,02 sous le dixième — le rang y est
    // quasi arbitraire, et CLAP encode le timbre, pas l'humeur. On traite donc
    // le cosinus comme une PRÉSÉLECTION large, puis on re-classe la file sur
    // des grandeurs déjà en base : le saut d'énergie (ReplayGain) et de tempo
    // (BPM) entre pistes consécutives.
    if let Some(tid) = seed_track_id {
        let pool = count.saturating_mul(3).max(50);
        let mut neigh = crate::audio::embedding_store::acoustic_neighbors(backend, tid, pool);
        if !neigh.is_empty() {
            let mut ids: Vec<i64> = neigh.iter().map(|(id, _)| *id).collect();
            ids.push(tid);
            let feats = load_queue_features(backend, &ids);
            let seed_feat = feats.get(&tid).cloned().unwrap_or_default();
            neigh = rerank_acoustic_queue(neigh, &seed_feat, &feats, count);
        }
        if !neigh.is_empty() {
            let ids: Vec<i64> = neigh.iter().map(|(id, _)| *id).collect();
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT {TRACK_COLS} \
                 FROM tracks t \
                 LEFT JOIN artists a ON t.artist_id = CAST(a.id AS TEXT) \
                 LEFT JOIN albums al ON t.album_id = CAST(al.id AS TEXT) \
                 WHERE t.id IN ({placeholders})"
            );
            let params: Vec<&dyn ToSqlValue> = ids.iter().map(|id| id as &dyn ToSqlValue).collect();
            if let Ok(rows) = backend.query_many(&sql, &params) {
                // SQL `IN` loses ordering; re-emit in descending-cosine order.
                for (id, _) in &neigh {
                    if let Some(r) = rows
                        .iter()
                        .find(|r| r.first().and_then(|v| v.as_i64()) == Some(*id))
                    {
                        results.push(row_to_track(r, "acoustic"));
                    }
                }
            }
        }
    }

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

    // Acoustic, genre and co-occurrence sources can surface the same track;
    // keep the first (highest-priority) occurrence and cap at the request.
    dedup_and_cap(&mut results, count);

    info!(count = results.len(), "smart_radio_generated");
    results
}

// ---------------------------------------------------------------------------
// Re-classement de la file acoustique (#1820)
// ---------------------------------------------------------------------------

/// Grandeurs auxiliaires d'une piste pour le re-classement de file : ce que la
/// bibliothèque sait DÉJÀ, sans nouveau modèle. `gain_db` vient du ReplayGain
/// mesuré (`rg_track_gain`), approximation grossière mais réelle de l'énergie ;
/// `bpm` vient des tags quand ils le portent.
#[derive(Debug, Clone, Default)]
struct QueueFeatures {
    gain_db: Option<f64>,
    bpm: Option<f64>,
}

/// Coût d'un saut d'énergie, en équivalent-cosinus par dB d'écart ReplayGain.
///
/// Étalonné sur la mesure du ticket : les dix premiers voisins d'une piste
/// tiennent dans une bande de ~0,09 de cosinus, et l'écart 10ᵉ/11ᵉ est < 0,02.
/// À 0,01/dB, un saut de 9 dB (un vrai retournement d'ambiance) coûte toute la
/// largeur de la bande, quand 1–2 dB de variation normale restent sous le
/// bruit du classement.
const ENERGY_WEIGHT_PER_DB: f32 = 0.01;

/// Coût d'un saut de tempo, en équivalent-cosinus par BPM d'écart : une
/// ballade à 70 BPM suivie d'un morceau à 130 coûte 0,06 — de quoi dominer un
/// écart de cosinus insignifiant, sans jamais écraser un vrai voisin.
const TEMPO_WEIGHT_PER_BPM: f32 = 0.001;

/// Charge les grandeurs auxiliaires d'un lot de pistes en une requête.
fn load_queue_features(
    backend: &Arc<dyn DbBackend>,
    ids: &[i64],
) -> std::collections::HashMap<i64, QueueFeatures> {
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT t.id, t.bpm, m.value \
         FROM tracks t \
         LEFT JOIN track_metadata m \
           ON m.track_id = t.id AND m.key = 'rg_track_gain' \
         WHERE t.id IN ({placeholders})"
    );
    let params: Vec<&dyn ToSqlValue> = ids.iter().map(|id| id as &dyn ToSqlValue).collect();
    if let Ok(rows) = backend.query_many(&sql, &params) {
        for r in &rows {
            let Some(id) = r.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            out.insert(
                id,
                QueueFeatures {
                    bpm: r.get(1).and_then(|v| v.as_f64()).filter(|b| *b > 0.0),
                    gain_db: r
                        .get(2)
                        .and_then(|v| v.as_string())
                        .and_then(crate::audio::replaygain::parse_gain_db),
                },
            );
        }
    }
    out
}

/// Pénalité de transition entre deux pistes consécutives, en équivalent-cosinus.
///
/// Une grandeur absente d'un des deux côtés ne pénalise pas : une bibliothèque
/// sans ReplayGain ni BPM retombe exactement sur l'ordre cosinus d'avant —
/// zéro régression.
fn transition_penalty(prev: &QueueFeatures, next: &QueueFeatures) -> f32 {
    let energy = match (prev.gain_db, next.gain_db) {
        (Some(a), Some(b)) => (a - b).abs() as f32 * ENERGY_WEIGHT_PER_DB,
        _ => 0.0,
    };
    let tempo = match (prev.bpm, next.bpm) {
        (Some(a), Some(b)) => (a - b).abs() as f32 * TEMPO_WEIGHT_PER_BPM,
        _ => 0.0,
    };
    energy + tempo
}

/// Re-classe une présélection cosinus en file cohérente (#1820).
///
/// Le cosinus CLAP constitue le vivier — il sait dire « plausible », pas
/// ordonner : le rang dans le top-10 est quasi arbitraire (bande de ~0,09,
/// dix-sept candidats à < 0,02 du dernier élu). Ici, chaîne gloutonne : à
/// chaque pas, la piste retenue est celle qui maximise
/// `cosinus − pénalité de transition` avec la piste PRÉCÉDENTE de la file
/// (la graine au premier pas). Déterministe : égalité tranchée par cosinus
/// décroissant puis id croissant.
fn rerank_acoustic_queue(
    mut pool: Vec<(i64, f32)>,
    seed: &QueueFeatures,
    feats: &std::collections::HashMap<i64, QueueFeatures>,
    count: usize,
) -> Vec<(i64, f32)> {
    let mut out = Vec::with_capacity(count.min(pool.len()));
    let mut prev = seed.clone();
    while out.len() < count && !pool.is_empty() {
        let default = QueueFeatures::default();
        let best = pool
            .iter()
            .enumerate()
            .map(|(i, (id, cos))| {
                let f = feats.get(id).unwrap_or(&default);
                (i, *cos - transition_penalty(&prev, f))
            })
            .max_by(|(ia, sa), (ib, sb)| {
                sa.total_cmp(sb)
                    // Égalité de score : plus fort cosinus, puis plus petit id.
                    .then(pool[*ia].1.total_cmp(&pool[*ib].1))
                    .then(pool[*ib].0.cmp(&pool[*ia].0))
            })
            .map(|(i, _)| i);
        let Some(i) = best else { break };
        let picked = pool.remove(i);
        prev = feats.get(&picked.0).cloned().unwrap_or_default();
        out.push(picked);
    }
    out
}

/// Deduplicate a radio queue in place, preserving priority order, then cap it.
///
/// Two keys are collapsed: the track id, and a normalised `artist \u{1} title`
/// content key. The content key is what stops duplicate rips / alternate
/// versions of the *same recording* (a library that imported an album twice)
/// from playing back-to-back — id dedup alone misses those, since they carry
/// distinct track ids. A track with an empty title has no reliable content key
/// and is kept on its (already unique) id.
fn dedup_and_cap(results: &mut Vec<RecommendedTrack>, count: usize) {
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_keys = std::collections::HashSet::new();
    results.retain(|t| {
        if !seen_ids.insert(t.track_id) {
            return false;
        }
        let title = t.title.trim();
        if title.is_empty() {
            return true;
        }
        let key = format!(
            "{}\u{1}{}",
            t.artist.as_deref().unwrap_or("").trim().to_lowercase(),
            title.to_lowercase()
        );
        seen_keys.insert(key)
    });
    results.truncate(count);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: i64, artist: &str, title: &str) -> RecommendedTrack {
        RecommendedTrack {
            track_id: id,
            title: title.to_string(),
            artist: (!artist.is_empty()).then(|| artist.to_string()),
            album: None,
            genre: None,
            duration_ms: 0,
            cover_path: None,
            reason: "acoustic".to_string(),
        }
    }

    fn qf(gain_db: Option<f64>, bpm: Option<f64>) -> QueueFeatures {
        QueueFeatures { gain_db, bpm }
    }

    fn feats(
        entries: &[(i64, Option<f64>, Option<f64>)],
    ) -> std::collections::HashMap<i64, QueueFeatures> {
        entries.iter().map(|(id, g, b)| (*id, qf(*g, *b))).collect()
    }

    // Le cas du ticket : deux candidats indiscernables au cosinus (0,91 vs
    // 0,90 — sous la largeur de bande mesurée), mais l'un saute de 12 dB
    // d'énergie. Le cosinus brut mettait l'intrus en tête ; le re-classement
    // met la piste d'énergie voisine d'abord.
    #[test]
    fn rerank_demotes_the_opposite_energy_intruder() {
        let pool = vec![(2, 0.91_f32), (1, 0.90_f32)];
        let seed = qf(Some(-5.0), None);
        let f = feats(&[(1, Some(-5.0), None), (2, Some(-17.0), None)]);
        let out = rerank_acoustic_queue(pool, &seed, &f, 2);
        assert_eq!(out.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 2]);
    }

    // Même chose sur le tempo : ballade (70 BPM) comme graine, un voisin à
    // 72 BPM passe devant un voisin à 140 BPM au cosinus à peine supérieur.
    #[test]
    fn rerank_demotes_the_opposite_tempo_intruder() {
        let pool = vec![(2, 0.91_f32), (1, 0.90_f32)];
        let seed = qf(None, Some(70.0));
        let f = feats(&[(1, None, Some(72.0)), (2, None, Some(140.0))]);
        let out = rerank_acoustic_queue(pool, &seed, &f, 2);
        assert_eq!(out.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 2]);
    }

    // La pénalité se calcule contre la piste PRÉCÉDENTE, pas seulement la
    // graine : la chaîne regroupe les énergies voisines au lieu d'alterner.
    #[test]
    fn rerank_chains_on_the_previous_track_not_the_seed() {
        // Graine calme (-15 dB). Deux fortes (0 dB), deux calmes (-15 dB).
        let pool = vec![
            (10, 0.95_f32), // forte
            (11, 0.94_f32), // calme
            (12, 0.93_f32), // forte
            (13, 0.92_f32), // calme
        ];
        let seed = qf(Some(-15.0), None);
        let f = feats(&[
            (10, Some(0.0), None),
            (11, Some(-15.0), None),
            (12, Some(0.0), None),
            (13, Some(-15.0), None),
        ]);
        let out = rerank_acoustic_queue(pool, &seed, &f, 4);
        // Les calmes d'abord (proches de la graine), puis les fortes par
        // cosinus — une fois la file passée côté fort, elle y reste.
        assert_eq!(
            out.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [11, 13, 10, 12]
        );
    }

    // Sans aucune grandeur auxiliaire (bibliothèque sans ReplayGain ni BPM),
    // l'ordre cosinus d'origine est conservé à l'identique — zéro régression.
    #[test]
    fn rerank_without_features_is_the_cosine_order() {
        let pool = vec![(7, 0.93_f32), (3, 0.91_f32), (9, 0.90_f32)];
        let out = rerank_acoustic_queue(pool.clone(), &QueueFeatures::default(), &feats(&[]), 3);
        assert_eq!(out, pool);
    }

    // Une grandeur absente d'UN côté ne pénalise pas : une piste non analysée
    // n'est pas punie face à une piste analysée.
    #[test]
    fn missing_feature_on_one_side_costs_nothing() {
        assert_eq!(
            transition_penalty(&qf(Some(-5.0), None), &qf(None, Some(120.0))),
            0.0
        );
    }

    // Déterministe : deux exécutions sur le même vivier donnent la même file,
    // et le vivier est tronqué à la demande.
    #[test]
    fn rerank_is_deterministic_and_caps_at_count() {
        let pool = vec![(5, 0.90_f32), (2, 0.90_f32), (8, 0.90_f32)];
        let f = feats(&[]);
        let a = rerank_acoustic_queue(pool.clone(), &QueueFeatures::default(), &f, 2);
        let b = rerank_acoustic_queue(pool.clone(), &QueueFeatures::default(), &f, 2);
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
        // Égalité parfaite de score et de cosinus : le plus petit id gagne.
        assert_eq!(a.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [2, 5]);
    }

    #[test]
    fn dedup_collapses_duplicate_rips_by_content_key() {
        // Same recording imported twice → distinct ids, identical artist/title.
        let mut v = vec![
            track(49457, "The Dave Brubeck Quartet", "Kathy's Waltz"),
            track(49464, "The Dave Brubeck Quartet", "Kathy's Waltz"),
            track(49312, "The Oscar Peterson Trio", "D. & E."),
        ];
        dedup_and_cap(&mut v, 30);
        assert_eq!(
            v.iter().map(|t| t.track_id).collect::<Vec<_>>(),
            vec![49457, 49312]
        );
    }

    #[test]
    fn dedup_is_case_and_whitespace_insensitive() {
        let mut v = vec![
            track(1, "Diana Krall", "Stop This World"),
            track(2, "  diana krall ", "  STOP THIS WORLD  "),
        ];
        dedup_and_cap(&mut v, 30);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn dedup_keeps_distinct_songs_and_same_title_across_artists() {
        let mut v = vec![
            track(1, "The Oscar Peterson Trio", "The Girl From Ipanema"),
            track(2, "Stan Getz", "The Girl From Ipanema"), // same title, other artist → kept
            track(3, "The Oscar Peterson Trio", "Corcovado"),
        ];
        dedup_and_cap(&mut v, 30);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn dedup_keeps_empty_title_tracks_on_id() {
        let mut v = vec![track(1, "", ""), track(2, "", "")];
        dedup_and_cap(&mut v, 30);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn dedup_preserves_priority_order_then_caps() {
        let mut v = vec![
            track(1, "A", "one"),
            track(2, "B", "two"),
            track(3, "C", "three"),
        ];
        dedup_and_cap(&mut v, 2);
        assert_eq!(v.iter().map(|t| t.track_id).collect::<Vec<_>>(), vec![1, 2]);
    }
}

#[cfg(test)]
mod smart_radio_rerank_tests {
    use super::*;
    use crate::audio::embedding_store::{EMBED_DIM, MODEL_ID, to_bytes};
    use crate::db::models::Track;
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_metadata_repo::TrackMetadataRepo;
    use crate::db::track_repo::TrackRepo;

    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Une piste analysée : embedding normé `[cos θ, sin θ, 0, …]` (cosinus
    /// exact avec la graine `[1, 0, …]`) + ReplayGain mesuré.
    fn mk_analysed(backend: &Arc<dyn DbBackend>, title: &str, cos: f32, gain: &str) -> i64 {
        let repo = TrackRepo::with_backend(backend.clone());
        let mut t = Track::new(title.into());
        t.format = Some("flac".into());
        t.duration_ms = 200_000;
        t.file_path = Some(format!("/m/{title}.flac"));
        let id = repo.create(&t).unwrap();

        let mut v = vec![0.0f32; EMBED_DIM];
        v[0] = cos;
        v[1] = (1.0 - cos * cos).max(0.0).sqrt();
        let blob = Some(to_bytes(&v));
        let params: [&dyn ToSqlValue; 3] = [&id, &MODEL_ID, &blob];
        backend
            .execute(
                "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                 VALUES (?, ?, ?, 42)",
                &params,
            )
            .unwrap();
        TrackMetadataRepo::with_backend(backend.clone())
            .set(id, "rg_track_gain", gain)
            .unwrap();
        id
    }

    // Bout en bout, le cas de DEvir (#1820) : deux voisins indiscernables au
    // cosinus (0,91 vs 0,90) dont l'un saute de 12 dB d'énergie. Le tri
    // cosinus brut met l'intrus en tête ; la file de la radio le repousse
    // derrière la piste d'énergie voisine.
    #[test]
    fn smart_radio_reorders_the_opposite_energy_neighbor() {
        let backend = setup();
        let seed = mk_analysed(&backend, "graine", 1.0, "-5.00 dB");
        let close = mk_analysed(&backend, "meme energie", 0.90, "-5.00 dB");
        let intruder = mk_analysed(&backend, "energie opposee", 0.91, "-17.00 dB");

        // Contre-épreuve interne : la présélection cosinus, elle, met bien
        // l'intrus d'abord — c'est le comportement que le ticket décrit.
        let raw = crate::audio::embedding_store::acoustic_neighbors(&backend, seed, 2);
        assert_eq!(
            raw.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [intruder, close],
            "le cosinus brut doit préférer l'intrus, sinon le test ne prouve rien"
        );

        let queue = smart_radio(&backend, Some(seed), None, None, 2);
        assert_eq!(
            queue.iter().map(|t| t.track_id).collect::<Vec<_>>(),
            [close, intruder],
            "la file re-classée doit enchaîner les énergies voisines"
        );
        assert!(queue.iter().all(|t| t.reason == "acoustic"));
    }

    // Bibliothèque analysée mais sans ReplayGain : l'ordre cosinus est
    // conservé tel quel — le re-classement n'invente rien.
    #[test]
    fn smart_radio_without_replaygain_keeps_cosine_order() {
        let backend = setup();
        let repo = TrackRepo::with_backend(backend.clone());
        let seed = {
            let mut t = Track::new("graine".into());
            t.format = Some("flac".into());
            t.file_path = Some("/m/graine.flac".into());
            let id = repo.create(&t).unwrap();
            let mut v = vec![0.0f32; EMBED_DIM];
            v[0] = 1.0;
            let blob = Some(to_bytes(&v));
            let params: [&dyn ToSqlValue; 3] = [&id, &MODEL_ID, &blob];
            backend
                .execute(
                    "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                     VALUES (?, ?, ?, 42)",
                    &params,
                )
                .unwrap();
            id
        };
        // Pas de rg_track_gain nulle part : mk_analysed non utilisé.
        for (title, cos) in [("premier", 0.95f32), ("second", 0.85f32)] {
            let mut t = Track::new(title.into());
            t.format = Some("flac".into());
            t.file_path = Some(format!("/m/{title}.flac"));
            let id = repo.create(&t).unwrap();
            let mut v = vec![0.0f32; EMBED_DIM];
            v[0] = cos;
            v[1] = (1.0 - cos * cos).sqrt();
            let blob = Some(to_bytes(&v));
            let params: [&dyn ToSqlValue; 3] = [&id, &MODEL_ID, &blob];
            backend
                .execute(
                    "INSERT INTO track_audio_embedding (track_id, model, embedding, analyzed_at) \
                     VALUES (?, ?, ?, 42)",
                    &params,
                )
                .unwrap();
        }
        let queue = smart_radio(&backend, Some(seed), None, None, 2);
        let titles: Vec<_> = queue.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["premier", "second"]);
    }
}
