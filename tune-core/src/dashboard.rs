use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Week,
    Month,
    Year,
    All,
}

impl Period {
    pub fn cutoff_sql(&self) -> Option<String> {
        let days = match self {
            Period::Week => 7,
            Period::Month => 30,
            Period::Year => 365,
            Period::All => return None,
        };
        Some(format!("datetime('now', '-{days} days')"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    pub total_plays: i64,
    pub total_listened_ms: i64,
    pub total_listened_hours: f64,
    pub unique_tracks: i64,
    pub unique_albums: i64,
    pub unique_artists: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopEntry {
    pub name: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_path: Option<String>,
    pub play_count: i64,
    pub total_listened_ms: i64,
    pub last_played: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayStats {
    pub day: String,
    pub play_count: i64,
    pub total_listened_ms: i64,
    pub hours: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenreStats {
    pub genre: String,
    pub play_count: i64,
    pub total_listened_ms: i64,
}

pub struct DashboardService {
    db: SqliteDb,
}

impl DashboardService {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn stats(&self) -> Result<DashboardStats, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row(
            "SELECT \
               COUNT(*) AS total_plays, \
               COALESCE(SUM(listened_ms), 0) AS total_listened_ms, \
               COUNT(DISTINCT track_id) AS unique_tracks, \
               COUNT(DISTINCT album_title) AS unique_albums, \
               COUNT(DISTINCT artist_name) AS unique_artists \
             FROM playback_history",
            [],
            |row| {
                let total_ms: i64 = row.get(1)?;
                Ok(DashboardStats {
                    total_plays: row.get(0)?,
                    total_listened_ms: total_ms,
                    total_listened_hours: total_ms as f64 / 3_600_000.0,
                    unique_tracks: row.get(2)?,
                    unique_albums: row.get(3)?,
                    unique_artists: row.get(4)?,
                })
            },
        )
        .map_err(|e| e.to_string())
    }

    pub fn top_artists(&self, period: Period, limit: i64) -> Result<Vec<TopEntry>, String> {
        let conn = self.db.connection().lock().unwrap();
        let where_clause = period
            .cutoff_sql()
            .map(|c| format!("AND played_at >= {c}"))
            .unwrap_or_default();

        let sql = format!(
            "SELECT artist_name, COUNT(*) AS play_count, \
               COALESCE(SUM(listened_ms), 0) AS total_listened_ms, \
               MAX(played_at) AS last_played \
             FROM playback_history \
             WHERE artist_name IS NOT NULL AND artist_name != '' {where_clause} \
             GROUP BY artist_name \
             ORDER BY play_count DESC LIMIT ?"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(TopEntry {
                    name: row.get(0)?,
                    artist_name: None,
                    album_title: None,
                    cover_path: None,
                    play_count: row.get(1)?,
                    total_listened_ms: row.get(2)?,
                    last_played: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn top_albums(&self, period: Period, limit: i64) -> Result<Vec<TopEntry>, String> {
        let conn = self.db.connection().lock().unwrap();
        let where_clause = period
            .cutoff_sql()
            .map(|c| format!("AND ph.played_at >= {c}"))
            .unwrap_or_default();

        let sql = format!(
            "SELECT ph.album_title, ph.artist_name, \
               COUNT(*) AS play_count, \
               COALESCE(SUM(ph.listened_ms), 0) AS total_listened_ms, \
               MAX(ph.played_at) AS last_played \
             FROM playback_history ph \
             WHERE ph.album_title IS NOT NULL AND ph.album_title != '' {where_clause} \
             GROUP BY ph.album_title, ph.artist_name \
             ORDER BY play_count DESC LIMIT ?"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(TopEntry {
                    name: row.get::<_, String>(0)?,
                    artist_name: row.get(1)?,
                    album_title: None,
                    cover_path: None,
                    play_count: row.get(2)?,
                    total_listened_ms: row.get(3)?,
                    last_played: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn top_tracks(&self, period: Period, limit: i64) -> Result<Vec<TopEntry>, String> {
        let conn = self.db.connection().lock().unwrap();
        let where_clause = period
            .cutoff_sql()
            .map(|c| format!("AND ph.played_at >= {c}"))
            .unwrap_or_default();

        let sql = format!(
            "SELECT ph.track_title, ph.artist_name, ph.album_title, \
               COUNT(*) AS play_count, \
               COALESCE(SUM(ph.listened_ms), 0) AS total_listened_ms, \
               MAX(ph.played_at) AS last_played \
             FROM playback_history ph \
             WHERE ph.track_title IS NOT NULL AND ph.track_title != '' {where_clause} \
             GROUP BY ph.track_title, ph.artist_name, ph.album_title \
             ORDER BY play_count DESC LIMIT ?"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(TopEntry {
                    name: row.get::<_, String>(0)?,
                    artist_name: row.get(1)?,
                    album_title: row.get(2)?,
                    cover_path: None,
                    play_count: row.get(3)?,
                    total_listened_ms: row.get(4)?,
                    last_played: row.get(5)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn listening_history(&self, days: i64) -> Result<Vec<DayStats>, String> {
        let conn = self.db.connection().lock().unwrap();
        let sql = format!(
            "SELECT DATE(played_at) AS day, \
               COUNT(*) AS play_count, \
               COALESCE(SUM(listened_ms), 0) AS total_listened_ms \
             FROM playback_history \
             WHERE played_at >= datetime('now', '-{days} days') \
             GROUP BY DATE(played_at) \
             ORDER BY day"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                let ms: i64 = row.get(2)?;
                Ok(DayStats {
                    day: row.get(0)?,
                    play_count: row.get(1)?,
                    total_listened_ms: ms,
                    hours: ms as f64 / 3_600_000.0,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn genre_breakdown(&self, period: Period) -> Result<Vec<GenreStats>, String> {
        let conn = self.db.connection().lock().unwrap();
        let where_clause = period
            .cutoff_sql()
            .map(|c| format!("AND ph.played_at >= {c}"))
            .unwrap_or_default();

        let sql = format!(
            "SELECT COALESCE(t.genre, 'Unknown') AS genre, \
               COUNT(*) AS play_count, \
               COALESCE(SUM(ph.listened_ms), 0) AS total_listened_ms \
             FROM playback_history ph \
             LEFT JOIN tracks t ON t.id = ph.track_id \
             WHERE 1=1 {where_clause} \
             GROUP BY COALESCE(t.genre, 'Unknown') \
             ORDER BY play_count DESC"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(GenreStats {
                    genre: row.get(0)?,
                    play_count: row.get(1)?,
                    total_listened_ms: row.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn period_cutoff() {
        assert!(Period::Week.cutoff_sql().is_some());
        assert!(Period::Month.cutoff_sql().is_some());
        assert!(Period::Year.cutoff_sql().is_some());
        assert!(Period::All.cutoff_sql().is_none());
    }

    #[test]
    fn stats_struct_serialize() {
        let stats = DashboardStats {
            total_plays: 100,
            total_listened_ms: 3_600_000,
            total_listened_hours: 1.0,
            unique_tracks: 50,
            unique_albums: 10,
            unique_artists: 5,
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["total_plays"], 100);
        assert_eq!(json["total_listened_hours"], 1.0);
    }

    #[test]
    fn day_stats_hours() {
        let ds = DayStats {
            day: "2024-01-01".into(),
            play_count: 20,
            total_listened_ms: 7_200_000,
            hours: 7_200_000.0 / 3_600_000.0,
        };
        assert!((ds.hours - 2.0).abs() < 0.01);
    }

    #[test]
    fn top_entry_serialize() {
        let entry = TopEntry {
            name: "Artist".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            play_count: 42,
            total_listened_ms: 100_000,
            last_played: Some("2024-01-01".into()),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["play_count"], 42);
    }
}
