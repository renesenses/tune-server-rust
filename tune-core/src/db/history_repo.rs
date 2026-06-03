use std::sync::Arc;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for history_repo.
///
/// Simple builders are dialect-aware. `full_dashboard` is still
/// SQLite-only (300 LOC of inline format!() that need a dedicated
/// rewrite, see docs/PORTING-HISTORY-REPO-PLAN.md).
pub mod sql {
    use super::SqlDialect;

    const RECORD_COLS: &str =
        "id, track_id, title, artist_name, album_title, source, duration_ms, listened_at, zone_id";

    pub fn record<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO listen_history (track_id, title, artist_name, album_title, source, duration_ms, zone_id) VALUES ({}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7)
        )
    }

    pub fn recent<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {RECORD_COLS} FROM listen_history ORDER BY listened_at DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn recent_paginated<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {RECORD_COLS} FROM listen_history ORDER BY listened_at DESC LIMIT {} OFFSET {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_all() -> &'static str {
        "SELECT COUNT(*) FROM listen_history"
    }

    pub fn top_tracks<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT title, artist_name, COUNT(*) as plays FROM listen_history GROUP BY title, artist_name ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn top_artists<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history WHERE artist_name IS NOT NULL GROUP BY artist_name ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn top_albums<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT album_title, artist_name, COUNT(*) as plays FROM listen_history WHERE album_title IS NOT NULL GROUP BY album_title, artist_name ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn dashboard_total_duration() -> &'static str {
        "SELECT COALESCE(SUM(duration_ms), 0) FROM listen_history"
    }

    pub fn dashboard_unique_tracks() -> &'static str {
        "SELECT COUNT(DISTINCT title || COALESCE(artist_name, '')) FROM listen_history"
    }

    pub fn dashboard_unique_artists() -> &'static str {
        "SELECT COUNT(DISTINCT artist_name) FROM listen_history WHERE artist_name IS NOT NULL"
    }

    /// Daily aggregation. Uses the date helpers so both engines emit
    /// the right SQL (SQLite strftime / PG to_char + interval).
    pub fn listening_history<D: SqlDialect>(d: &D, days: i64) -> String {
        let day_col = d.date_trunc_day("listened_at");
        let since = d.since_days("listened_at", days);
        format!(
            "SELECT {day_col} as day, COUNT(*) as play_count, COALESCE(SUM(duration_ms), 0) as total_ms \
             FROM listen_history WHERE {since} \
             GROUP BY day ORDER BY day"
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenRecord {
    pub id: Option<i64>,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub source: String,
    pub duration_ms: i64,
    pub listened_at: Option<String>,
    pub zone_id: Option<i64>,
}

pub struct HistoryRepo {
    db: Arc<dyn DbBackend>,
    /// SQLite-specific handle for the `full_dashboard` method whose
    /// 300-LOC dynamic SQL hasn't been ported yet (see
    /// docs/PORTING-HISTORY-REPO-PLAN.md Group C). Populated when the
    /// repo is constructed via `new(SqliteDb)`; `None` when constructed
    /// via `with_backend(Arc<dyn DbBackend>)`, in which case
    /// `full_dashboard` returns an explicit "not implemented" error.
    /// Removed once `full_dashboard` is ported in a dedicated commit.
    sqlite_legacy: Option<SqliteDb>,
}

impl HistoryRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            sqlite_legacy: Some(db.clone()),
            db: Arc::new(db),
        }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self {
            db,
            sqlite_legacy: None,
        }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    pub fn record(&self, rec: &ListenRecord) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::record, sql::record);
        let params: [&dyn ToSqlValue; 7] = [
            &rec.track_id,
            &rec.title,
            &rec.artist_name,
            &rec.album_title,
            &rec.source,
            &rec.duration_ms,
            &rec.zone_id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<ListenRecord>, String> {
        let sql = self.dialect_sql(sql::recent, sql::recent);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_listen).collect())
    }

    pub fn recent_paginated(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ListenRecord>, i64), String> {
        let total = match self.db.query_one(sql::count_all(), &[])? {
            None => 0,
            Some(cols) => cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        };
        let sql = self.dialect_sql(sql::recent_paginated, sql::recent_paginated);
        let params: [&dyn ToSqlValue; 2] = [&limit, &offset];
        let rows = self.db.query_many(&sql, &params)?;
        Ok((rows.iter().map(row_to_listen).collect(), total))
    }

    pub fn top_tracks(&self, limit: i64) -> Result<Vec<(String, Option<String>, i64)>, String> {
        let sql = self.dialect_sql(sql::top_tracks, sql::top_tracks);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_string()),
                    cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn top_artists(&self, limit: i64) -> Result<Vec<(String, i64)>, String> {
        let sql = self.dialect_sql(sql::top_artists, sql::top_artists);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn top_albums(&self, limit: i64) -> Result<Vec<(String, Option<String>, i64)>, String> {
        let sql = self.dialect_sql(sql::top_albums, sql::top_albums);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_string()),
                    cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn listening_history(&self, days: i64) -> Result<Vec<(String, i64, i64)>, String> {
        let sql = self.dialect_sql(
            |d| sql::listening_history(d, days),
            |d| sql::listening_history(d, days),
        );
        let rows = self.db.query_many(&sql, &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                    cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count_all(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }

    pub fn dashboard(&self) -> Result<DashboardStats, String> {
        let total_listens = self
            .db
            .query_one(sql::count_all(), &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let total_duration_ms = self
            .db
            .query_one(sql::dashboard_total_duration(), &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let unique_tracks = self
            .db
            .query_one(sql::dashboard_unique_tracks(), &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let unique_artists = self
            .db
            .query_one(sql::dashboard_unique_artists(), &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(DashboardStats {
            total_listens,
            total_duration_ms,
            unique_tracks,
            unique_artists,
        })
    }

    /// Rich dashboard. **Not yet ported to `Arc<dyn DbBackend>`** —
    /// uses the SQLite legacy handle (300 LOC of dynamic SQL with 10
    /// sub-queries to abstract). Returns an error when constructed via
    /// `with_backend`. Plan: docs/PORTING-HISTORY-REPO-PLAN.md Group C.
    pub fn full_dashboard(
        &self,
        period: &str,
        zone_id: Option<i64>,
        _profile_id: Option<i64>,
        top_n: i64,
    ) -> Result<DashboardData, String> {
        let db = self.sqlite_legacy.as_ref().ok_or_else(|| {
            "full_dashboard: not yet implemented for non-SQLite backends \
             (docs/PORTING-HISTORY-REPO-PLAN.md Group C)"
                .to_string()
        })?;
        let conn = db.read_connection().lock().unwrap();

        let mut conditions: Vec<String> = Vec::new();
        let days: Option<i64> = match period {
            "7d" => Some(7),
            "30d" => Some(30),
            "90d" => Some(90),
            "all" => None,
            other => other.trim_end_matches('d').parse::<i64>().ok(),
        };
        if let Some(d) = days {
            conditions.push(format!(
                "listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{d} days')"
            ));
        }
        if let Some(zid) = zone_id {
            conditions.push(format!("h.zone_id = {zid}"));
        }
        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let mut simple_conditions: Vec<String> = Vec::new();
        if let Some(d) = days {
            simple_conditions.push(format!(
                "listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{d} days')"
            ));
        }
        if let Some(zid) = zone_id {
            simple_conditions.push(format!("zone_id = {zid}"));
        }
        let simple_where = if simple_conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", simple_conditions.join(" AND "))
        };

        let from: Option<String> = if days.is_some() {
            conn.query_row(
                &format!("SELECT MIN(listened_at) FROM listen_history {simple_where}"),
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten()
        } else {
            conn.query_row("SELECT MIN(listened_at) FROM listen_history", [], |row| {
                row.get(0)
            })
            .ok()
            .flatten()
        };
        let to: String = conn
            .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
                row.get(0)
            })
            .map_err(|e| e.to_string())?;

        let (plays, listening_ms, u_tracks, u_artists): (i64, i64, i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*),
                            COALESCE(SUM(duration_ms), 0),
                            COUNT(DISTINCT title || COALESCE(artist_name, '')),
                            COUNT(DISTINCT CASE WHEN artist_name IS NOT NULL THEN artist_name END)
                     FROM listen_history {simple_where}"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| e.to_string())?;

        let top_artists = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT artist_name, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
                     FROM listen_history
                     {simple_where} {and_or} artist_name IS NOT NULL
                     GROUP BY artist_name ORDER BY plays DESC LIMIT ?",
                    and_or = if simple_where.is_empty() {
                        "WHERE"
                    } else {
                        "AND"
                    },
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map(params![top_n], |row| {
                Ok(TopArtistEntry {
                    artist_name: row.get(0)?,
                    plays: row.get(1)?,
                    listening_ms: row.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let top_albums = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT h.album_title, h.artist_name, a.cover_path, COUNT(*) as plays
                     FROM listen_history h
                     LEFT JOIN albums a ON a.title = h.album_title
                     {where_clause} {and_or} h.album_title IS NOT NULL
                     GROUP BY h.album_title, h.artist_name
                     ORDER BY plays DESC LIMIT ?",
                    and_or = if where_clause.is_empty() {
                        "WHERE"
                    } else {
                        "AND"
                    },
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map(params![top_n], |row| {
                Ok(TopAlbumEntry {
                    album_title: row.get(0)?,
                    artist_name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    cover_path: row.get(2)?,
                    plays: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let top_tracks = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT track_id, title, artist_name, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
                     FROM listen_history
                     {simple_where}
                     GROUP BY title, artist_name ORDER BY plays DESC LIMIT ?"
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map(params![top_n], |row| {
                Ok(TopTrackEntry {
                    track_id: row.get(0)?,
                    title: row.get(1)?,
                    artist_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    plays: row.get(3)?,
                    listening_ms: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let trend = {
            let trend_days = days.unwrap_or(365);
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT DATE(listened_at) as day, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
                     FROM listen_history
                     WHERE listened_at >= strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{trend_days} days')
                     {zone_and}
                     GROUP BY day ORDER BY day",
                    zone_and = if let Some(zid) = zone_id {
                        format!("AND zone_id = {zid}")
                    } else {
                        String::new()
                    },
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| {
                Ok(TrendEntry {
                    day: row.get(0)?,
                    plays: row.get(1)?,
                    listening_ms: row.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let hourly = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT CAST(strftime('%H', listened_at) AS INTEGER) as hour, COUNT(*) as plays
                     FROM listen_history
                     {simple_where}
                     GROUP BY hour ORDER BY hour"
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| {
                Ok(HourlyEntry {
                    hour: row.get(0)?,
                    plays: row.get(1)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let by_zone = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT h.zone_id, z.name, COUNT(*) as plays, COALESCE(SUM(h.duration_ms), 0) as ms
                     FROM listen_history h
                     LEFT JOIN zones z ON z.id = h.zone_id
                     {where_clause}
                     GROUP BY h.zone_id ORDER BY plays DESC"
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| {
                Ok(ByZoneEntry {
                    zone_id: row.get(0)?,
                    zone_name: row.get(1)?,
                    plays: row.get(2)?,
                    listening_ms: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let by_source = {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT source, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
                     FROM listen_history
                     {simple_where}
                     GROUP BY source ORDER BY plays DESC"
                ))
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| {
                Ok(BySourceEntry {
                    source: row.get(0)?,
                    plays: row.get(1)?,
                    listening_ms: row.get(2)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        let completion = conn
            .query_row(
                &format!(
                    "SELECT
                        COUNT(CASE WHEN duration_ms >= 30000 THEN 1 END),
                        COUNT(CASE WHEN duration_ms < 30000 THEN 1 END),
                        COALESCE(AVG(duration_ms), 0),
                        COALESCE((SELECT AVG(t.duration_ms) FROM listen_history lh
                                  LEFT JOIN tracks t ON t.id = lh.track_id
                                  {simple_where_inner} {and_or_inner} t.duration_ms IS NOT NULL), 0)
                     FROM listen_history
                     {simple_where}",
                    simple_where_inner = if simple_conditions.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "WHERE {}",
                            simple_conditions
                                .iter()
                                .map(|c| format!("lh.{c}"))
                                .collect::<Vec<_>>()
                                .join(" AND ")
                        )
                    },
                    and_or_inner = if simple_conditions.is_empty() {
                        "WHERE"
                    } else {
                        "AND"
                    },
                ),
                [],
                |row| {
                    Ok(CompletionStats {
                        completed: row.get(0)?,
                        skipped: row.get(1)?,
                        avg_listened_ms: row.get::<_, f64>(2).unwrap_or(0.0) as i64,
                        avg_track_duration_ms: row.get::<_, f64>(3).unwrap_or(0.0) as i64,
                    })
                },
            )
            .map_err(|e| e.to_string())?;

        Ok(DashboardData {
            period: period.to_string(),
            range: DashboardRange { from, to },
            totals: DashboardTotals {
                plays,
                listening_ms,
                unique_tracks: u_tracks,
                unique_artists: u_artists,
            },
            top_artists,
            top_albums,
            top_tracks,
            trend,
            hourly,
            by_zone,
            by_source,
            completion,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardStats {
    pub total_listens: i64,
    pub total_duration_ms: i64,
    pub unique_tracks: i64,
    pub unique_artists: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardRange {
    pub from: Option<String>,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardTotals {
    pub plays: i64,
    pub listening_ms: i64,
    pub unique_tracks: i64,
    pub unique_artists: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopArtistEntry {
    pub artist_name: String,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopAlbumEntry {
    pub album_title: String,
    pub artist_name: String,
    pub cover_path: Option<String>,
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopTrackEntry {
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: String,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendEntry {
    pub day: String,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlyEntry {
    pub hour: i64,
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByZoneEntry {
    pub zone_id: Option<i64>,
    pub zone_name: Option<String>,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BySourceEntry {
    pub source: Option<String>,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionStats {
    pub completed: i64,
    pub skipped: i64,
    pub avg_listened_ms: i64,
    pub avg_track_duration_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardData {
    pub period: String,
    pub range: DashboardRange,
    pub totals: DashboardTotals,
    pub top_artists: Vec<TopArtistEntry>,
    pub top_albums: Vec<TopAlbumEntry>,
    pub top_tracks: Vec<TopTrackEntry>,
    pub trend: Vec<TrendEntry>,
    pub hourly: Vec<HourlyEntry>,
    pub by_zone: Vec<ByZoneEntry>,
    pub by_source: Vec<BySourceEntry>,
    pub completion: CompletionStats,
}

fn row_to_listen(cols: &Vec<SqlValue>) -> ListenRecord {
    ListenRecord {
        id: cols.first().and_then(|v| v.as_i64()),
        track_id: cols.get(1).and_then(|v| v.as_i64()),
        title: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        artist_name: cols.get(3).and_then(|v| v.as_string()),
        album_title: cols.get(4).and_then(|v| v.as_string()),
        source: cols
            .get(5)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "local".into()),
        duration_ms: cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
        listened_at: cols.get(7).and_then(|v| v.as_string()),
        zone_id: cols.get(8).and_then(|v| v.as_i64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_repo() -> HistoryRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        HistoryRepo::new(db)
    }

    #[test]
    fn record_and_query_history() {
        let repo = fresh_repo();
        let rec = ListenRecord {
            id: None,
            track_id: None,
            title: "So What".into(),
            artist_name: Some("Miles Davis".into()),
            album_title: Some("Kind of Blue".into()),
            source: "local".into(),
            duration_ms: 562_000,
            listened_at: None,
            zone_id: None,
        };

        repo.record(&rec).unwrap();
        repo.record(&rec).unwrap();

        let recent = repo.recent(10).unwrap();
        assert_eq!(recent.len(), 2);

        let top = repo.top_tracks(5).unwrap();
        assert_eq!(top[0].0, "So What");
        assert_eq!(top[0].2, 2);

        let dashboard = repo.dashboard().unwrap();
        assert_eq!(dashboard.total_listens, 2);
        assert_eq!(dashboard.unique_tracks, 1);
    }

    #[test]
    fn history_top_artists() {
        let repo = fresh_repo();
        for _ in 0..5 {
            repo.record(&ListenRecord {
                id: None,
                track_id: None,
                title: "Song A".into(),
                artist_name: Some("Miles Davis".into()),
                album_title: None,
                source: "local".into(),
                duration_ms: 300_000,
                listened_at: None,
                zone_id: None,
            })
            .unwrap();
        }
        for _ in 0..3 {
            repo.record(&ListenRecord {
                id: None,
                track_id: None,
                title: "Song B".into(),
                artist_name: Some("Coltrane".into()),
                album_title: None,
                source: "local".into(),
                duration_ms: 400_000,
                listened_at: None,
                zone_id: None,
            })
            .unwrap();
        }

        let top = repo.top_artists(10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "Miles Davis");
        assert_eq!(top[0].1, 5);
        assert_eq!(top[1].0, "Coltrane");
        assert_eq!(top[1].1, 3);
    }

    #[test]
    fn history_top_albums() {
        let repo = fresh_repo();
        for _ in 0..3 {
            repo.record(&ListenRecord {
                id: None,
                track_id: None,
                title: "Track".into(),
                artist_name: Some("Miles".into()),
                album_title: Some("Kind of Blue".into()),
                source: "local".into(),
                duration_ms: 300_000,
                listened_at: None,
                zone_id: None,
            })
            .unwrap();
        }

        let top = repo.top_albums(10).unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, "Kind of Blue");
        assert_eq!(top[0].2, 3);
    }

    #[test]
    fn history_count() {
        let repo = fresh_repo();
        assert_eq!(repo.count().unwrap(), 0);

        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "A".into(),
            artist_name: None,
            album_title: None,
            source: "local".into(),
            duration_ms: 0,
            listened_at: None,
            zone_id: None,
        })
        .unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn history_dashboard_total_duration() {
        let repo = fresh_repo();
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "A".into(),
            artist_name: Some("X".into()),
            album_title: None,
            source: "local".into(),
            duration_ms: 300_000,
            listened_at: None,
            zone_id: None,
        })
        .unwrap();
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "B".into(),
            artist_name: Some("Y".into()),
            album_title: None,
            source: "tidal".into(),
            duration_ms: 200_000,
            listened_at: None,
            zone_id: None,
        })
        .unwrap();

        let dash = repo.dashboard().unwrap();
        assert_eq!(dash.total_duration_ms, 500_000);
        assert_eq!(dash.unique_tracks, 2);
        assert_eq!(dash.unique_artists, 2);
    }

    #[test]
    fn history_recent_order() {
        let repo = fresh_repo();
        for title in ["First", "Second", "Third"] {
            repo.record(&ListenRecord {
                id: None,
                track_id: None,
                title: title.into(),
                artist_name: None,
                album_title: None,
                source: "local".into(),
                duration_ms: 0,
                listened_at: None,
                zone_id: None,
            })
            .unwrap();
        }

        let recent = repo.recent(10).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].title, "Third");
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::record(&s).contains("VALUES (?, ?, ?, ?, ?, ?, ?)"));
        assert!(sql::record(&p).contains("VALUES ($1, $2, $3, $4, $5, $6, $7)"));
        assert!(sql::recent_paginated(&p).contains("LIMIT $1 OFFSET $2"));
        assert!(sql::listening_history(&p, 7).contains("interval '7 days'"));
        assert!(sql::listening_history(&s, 7).contains("'-7 days'"));
    }

    #[test]
    fn history_with_zone_id() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db.execute("INSERT INTO zones (name) VALUES ('Main')", &[])
            .unwrap();

        let repo = HistoryRepo::new(db);
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "Test".into(),
            artist_name: None,
            album_title: None,
            source: "local".into(),
            duration_ms: 100_000,
            listened_at: None,
            zone_id: Some(1),
        })
        .unwrap();

        let recent = repo.recent(1).unwrap();
        assert_eq!(recent[0].zone_id, Some(1));
    }

    #[test]
    fn with_backend_constructor_rejects_full_dashboard() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = HistoryRepo::with_backend(backend);
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "via-backend".into(),
            artist_name: None,
            album_title: None,
            source: "local".into(),
            duration_ms: 0,
            listened_at: None,
            zone_id: None,
        })
        .unwrap();
        assert_eq!(repo.count().unwrap(), 1);
        assert!(repo.full_dashboard("7d", None, None, 10).is_err());
    }
}
