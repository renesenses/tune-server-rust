use std::sync::Arc;

use serde::Serialize;
use tracing::info;

use crate::db::backend::{DbBackend, ToSqlValue};

// ---------------------------------------------------------------------------
// DigestReport — the full weekly listening report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ArtistPlay {
    pub name: String,
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackPlay {
    pub title: String,
    pub artist: Option<String>,
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Up,
    Down,
    Same,
}

#[derive(Debug, Clone, Serialize)]
pub struct DigestReport {
    pub week_start: String,
    pub week_end: String,
    pub total_plays: i64,
    pub total_hours: f64,
    pub top_artists: Vec<ArtistPlay>,
    pub top_tracks: Vec<TrackPlay>,
    pub new_artists: Vec<String>,
    pub trend: Trend,
    pub trend_detail: TrendDetail,
    pub recommendations: Vec<TrackPlay>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrendDetail {
    pub this_week_plays: i64,
    pub last_week_plays: i64,
    pub diff: i64,
}

// ---------------------------------------------------------------------------
// generate_digest — compute the weekly report from listen_history
// ---------------------------------------------------------------------------

pub fn generate_digest(backend: &Arc<dyn DbBackend>) -> Result<DigestReport, String> {
    // Date boundaries: this week = last 7 days, prev week = 14..7 days ago
    let now = chrono::Utc::now();
    let week_end = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let week_start_dt = now - chrono::Duration::days(7);
    let week_start = week_start_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let prev_week_start_dt = now - chrono::Duration::days(14);
    let prev_week_start = prev_week_start_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // --- Totals this week ---
    let totals = backend
        .query_one(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ?",
            &[&week_start as &dyn ToSqlValue, &week_end],
        )
        .map_err(|e| format!("digest totals: {e}"))?
        .unwrap_or_default();

    let total_plays = totals.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = totals.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
    let total_hours = (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0;

    // --- Top 5 artists this week ---
    let artist_rows = backend
        .query_many(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? AND artist_name IS NOT NULL \
             GROUP BY artist_name ORDER BY plays DESC LIMIT 5",
            &[&week_start as &dyn ToSqlValue, &week_end],
        )
        .unwrap_or_default();

    let top_artists: Vec<ArtistPlay> = artist_rows
        .iter()
        .map(|cols| ArtistPlay {
            name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
            plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        })
        .collect();

    // --- Top 5 tracks this week ---
    let track_rows = backend
        .query_many(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ? \
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT 5",
            &[&week_start as &dyn ToSqlValue, &week_end],
        )
        .unwrap_or_default();

    let top_tracks: Vec<TrackPlay> = track_rows
        .iter()
        .map(|cols| TrackPlay {
            title: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
            artist: cols.get(1).and_then(|v| v.as_string()),
            plays: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
        })
        .collect();

    // --- New artists discovered (first play ever was this week) ---
    let new_rows = backend
        .query_many(
            "SELECT artist_name FROM listen_history \
             WHERE artist_name IS NOT NULL \
             GROUP BY artist_name \
             HAVING MIN(listened_at) >= ? AND MIN(listened_at) < ? \
             ORDER BY COUNT(*) DESC LIMIT 10",
            &[&week_start as &dyn ToSqlValue, &week_end],
        )
        .unwrap_or_default();

    let new_artists: Vec<String> = new_rows
        .iter()
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();

    // --- Trend vs last week ---
    let prev_totals = backend
        .query_one(
            "SELECT COUNT(*) FROM listen_history \
             WHERE listened_at >= ? AND listened_at < ?",
            &[&prev_week_start as &dyn ToSqlValue, &week_start],
        )
        .map_err(|e| format!("digest prev week: {e}"))?
        .unwrap_or_default();

    let last_week_plays = prev_totals.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let diff = total_plays - last_week_plays;
    let trend = if diff > 0 {
        Trend::Up
    } else if diff < 0 {
        Trend::Down
    } else {
        Trend::Same
    };

    // --- Recommendations: tracks from top genres not in top tracks ---
    let recommendations = generate_recommendations(backend, &week_start, &week_end);

    let report = DigestReport {
        week_start: week_start_dt.format("%Y-%m-%d").to_string(),
        week_end: now.format("%Y-%m-%d").to_string(),
        total_plays,
        total_hours,
        top_artists,
        top_tracks,
        new_artists,
        trend,
        trend_detail: TrendDetail {
            this_week_plays: total_plays,
            last_week_plays,
            diff,
        },
        recommendations,
    };

    info!(
        plays = total_plays,
        hours = total_hours,
        artists = report.top_artists.len(),
        "digest_generated"
    );

    Ok(report)
}

// ---------------------------------------------------------------------------
// Simple genre-based recommendations
// ---------------------------------------------------------------------------

fn generate_recommendations(
    backend: &Arc<dyn DbBackend>,
    week_start: &str,
    week_end: &str,
) -> Vec<TrackPlay> {
    // Find the top genres played this week by joining with tracks table
    let genre_rows = backend
        .query_many(
            "SELECT t.genre FROM listen_history lh \
             INNER JOIN tracks t ON t.id = lh.track_id \
             WHERE lh.listened_at >= ? AND lh.listened_at < ? \
             AND t.genre IS NOT NULL AND t.genre != '' \
             GROUP BY t.genre ORDER BY COUNT(*) DESC LIMIT 3",
            &[
                &week_start.to_string() as &dyn ToSqlValue,
                &week_end.to_string(),
            ],
        )
        .unwrap_or_default();

    let top_genres: Vec<String> = genre_rows
        .iter()
        .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
        .collect();

    // #4806 — une suggestion du résumé hebdomadaire est une sélection
    // automatique : jamais un titre banni par le profil actif.
    let sans_bannis = crate::db::facet_filter::banned_tracks_excluded(
        crate::db::hidden_repo::profil_de_selection_automatique(backend),
    );
    if top_genres.is_empty() {
        // Fallback: random tracks from library not recently played
        let sql = format!(
            "SELECT t.title, a.name FROM tracks t \
             LEFT JOIN artists a ON a.id = t.artist_id \
             WHERE t.id NOT IN ( \
                 SELECT DISTINCT track_id FROM listen_history \
                 WHERE track_id IS NOT NULL AND listened_at >= ? \
             ) \
             AND {sans_bannis} \
             ORDER BY RANDOM() LIMIT 5"
        );
        return backend
            .query_many(&sql, &[&week_start.to_string() as &dyn ToSqlValue])
            .unwrap_or_default()
            .iter()
            .map(|cols| TrackPlay {
                title: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                artist: cols.get(1).and_then(|v| v.as_string()),
                plays: 0,
            })
            .collect();
    }

    // Find tracks in matching genres that weren't played this week
    let genre_filter = top_genres
        .iter()
        .map(|g| format!("t.genre LIKE '%{}%'", g.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(" OR ");

    let sql = format!(
        "SELECT t.title, a.name FROM tracks t \
         LEFT JOIN artists a ON a.id = t.artist_id \
         WHERE ({genre_filter}) \
         AND t.id NOT IN ( \
             SELECT DISTINCT track_id FROM listen_history \
             WHERE track_id IS NOT NULL AND listened_at >= ? \
         ) \
         AND {sans_bannis} \
         ORDER BY RANDOM() LIMIT 5"
    );

    backend
        .query_many(&sql, &[&week_start.to_string() as &dyn ToSqlValue])
        .unwrap_or_default()
        .iter()
        .map(|cols| TrackPlay {
            title: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
            artist: cols.get(1).and_then(|v| v.as_string()),
            plays: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests_titres_bannis_4806 {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    /// #4806 — les suggestions du résumé hebdomadaire sont une sélection
    /// automatique : un titre banni n'y figure jamais, avec ou sans
    /// historique de genre. Témoin avant, retour après débannissement.
    #[test]
    fn les_suggestions_du_resume_ignorent_un_titre_banni() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        for i in 1..=6i64 {
            let params: [&dyn ToSqlValue; 3] = [&i, &format!("Piste {i}"), &format!("/m/{i}.flac")];
            db.execute(
                "INSERT INTO tracks (id, title, file_path, genre) VALUES (?, ?, ?, 'Jazz')",
                &params,
            )
            .unwrap();
        }
        let suggestions = || -> Vec<String> {
            generate_recommendations(&db, "2026-09-14", "2026-09-21")
                .into_iter()
                .map(|t| t.title)
                .collect()
        };
        assert!(
            (0..50).any(|_| suggestions().contains(&"Piste 3".to_string())),
            "témoin : la piste sort avant le bannissement"
        );

        let bans = crate::db::hidden_repo::HiddenRepo::with_backend(db.clone());
        assert!(bans.ban_track(1, 3).unwrap());
        for _ in 0..50 {
            let s = suggestions();
            assert_eq!(s.len(), 5, "cinq suggestions jouables : {s:?}");
            assert!(
                !s.contains(&"Piste 3".to_string()),
                "bannie et suggérée : {s:?}"
            );
        }

        assert!(bans.unban_track(1, 3).unwrap());
        assert!((0..50).any(|_| suggestions().contains(&"Piste 3".to_string())));
    }
}
