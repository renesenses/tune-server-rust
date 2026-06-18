use std::sync::Arc;

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

    const RECORD_COLS: &str = "id, track_id, title, artist_name, album_title, source, source_id, album_id, duration_ms, listened_at, zone_id";

    pub fn record<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO listen_history (track_id, title, artist_name, album_title, source, source_id, album_id, duration_ms, zone_id, cover_url) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10)
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
            "SELECT h.title, h.artist_name, COUNT(*) as plays, \
             COALESCE(t.id, h.track_id) as track_id, \
             COALESCE(al.cover_path, al2.cover_path, MAX(h.cover_url)) as cover_path, \
             COALESCE(al.title, h.album_title) as album_title, \
             h.source \
             FROM listen_history h \
             LEFT JOIN tracks t ON h.track_id = t.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN albums al2 ON al2.title = h.album_title AND al2.cover_path IS NOT NULL \
             GROUP BY h.title, h.artist_name \
             ORDER BY plays DESC LIMIT {}",
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
    pub source_id: Option<String>,
    pub album_id: Option<i64>,
    pub duration_ms: i64,
    pub listened_at: Option<String>,
    pub zone_id: Option<i64>,
    pub cover_url: Option<String>,
}

pub struct HistoryRepo {
    db: Arc<dyn DbBackend>,
}

impl HistoryRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
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
        let params: [&dyn ToSqlValue; 10] = [
            &rec.track_id,
            &rec.title,
            &rec.artist_name,
            &rec.album_title,
            &rec.source,
            &rec.source_id,
            &rec.album_id,
            &rec.duration_ms,
            &rec.zone_id,
            &rec.cover_url,
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

    pub fn top_tracks(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let sql = self.dialect_sql(sql::top_tracks, sql::top_tracks);
        let params: [&dyn ToSqlValue; 1] = [&limit];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                serde_json::json!({
                    "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist_name": cols.get(1).and_then(|v| v.as_string()),
                    "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                    "track_id": cols.get(3).and_then(|v| v.as_i64()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "album_title": cols.get(5).and_then(|v| v.as_string()),
                    "source": cols.get(6).and_then(|v| v.as_string()),
                })
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

    /// Rich dashboard matching the web client `DashboardData` type.
    /// Now fully ported through `Arc<dyn DbBackend>` — uses the date
    /// dialect helpers (`since_days`, `date_trunc_day`, `extract_hour`,
    /// `now_iso8601`) so the same SQL emission paths work on both
    /// SQLite and Postgres.
    ///
    /// `period`: "7d", "30d", "90d", "all" (default 30d).
    /// `zone_id`: optional filter.
    /// `top_n`: how many items per top list.
    pub fn full_dashboard(
        &self,
        period: &str,
        zone_id: Option<i64>,
        _profile_id: Option<i64>,
        top_n: i64,
    ) -> Result<DashboardData, String> {
        let days: Option<i64> = match period {
            "7d" => Some(7),
            "30d" => Some(30),
            "90d" => Some(90),
            "all" => None,
            other => other.trim_end_matches('d').parse::<i64>().ok(),
        };

        // Build WHERE clauses using the date dialect helpers. We need
        // two flavors: `simple_where` for queries without table alias,
        // `aliased_where` for queries that use `h.` aliasing.
        let dialect_since = |col: &str, n: i64| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.since_days(col, n),
            Engine::Postgres => PostgresDialect.since_days(col, n),
        };
        let dialect_now = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.now_iso8601().to_string(),
            Engine::Postgres => PostgresDialect.now_iso8601().to_string(),
        };
        let dialect_day = |col: &str| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.date_trunc_day(col),
            Engine::Postgres => PostgresDialect.date_trunc_day(col),
        };
        let dialect_hour = |col: &str| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.extract_hour(col),
            Engine::Postgres => PostgresDialect.extract_hour(col),
        };

        let mut simple_conditions: Vec<String> = Vec::new();
        if let Some(d) = days {
            simple_conditions.push(dialect_since("listened_at", d));
        }
        if let Some(zid) = zone_id {
            simple_conditions.push(format!("zone_id = {zid}"));
        }
        let simple_where = if simple_conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", simple_conditions.join(" AND "))
        };

        let mut aliased_conditions: Vec<String> = Vec::new();
        if let Some(d) = days {
            aliased_conditions.push(dialect_since("listened_at", d));
        }
        if let Some(zid) = zone_id {
            aliased_conditions.push(format!("h.zone_id = {zid}"));
        }
        let where_clause = if aliased_conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", aliased_conditions.join(" AND "))
        };

        // ── Range ──
        let from_sql = if days.is_some() {
            format!("SELECT MIN(listened_at) FROM listen_history {simple_where}")
        } else {
            "SELECT MIN(listened_at) FROM listen_history".to_string()
        };
        let from: Option<String> = self
            .db
            .query_one(&from_sql, &[])
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_string()));
        let to: String = self
            .db
            .query_one(&format!("SELECT {dialect_now}"), &[])?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_string()))
            .unwrap_or_default();

        // ── Totals ──
        let totals_sql = format!(
            "SELECT COUNT(*),
                    COALESCE(SUM(duration_ms), 0),
                    COUNT(DISTINCT title || COALESCE(artist_name, '')),
                    COUNT(DISTINCT CASE WHEN artist_name IS NOT NULL THEN artist_name END)
             FROM listen_history {simple_where}"
        );
        let totals_row = self
            .db
            .query_one(&totals_sql, &[])?
            .ok_or("totals query returned no row")?;
        let plays = totals_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let listening_ms = totals_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
        let u_tracks = totals_row.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
        let u_artists = totals_row.get(3).and_then(|v| v.as_i64()).unwrap_or(0);

        // ── Top artists ──
        let artists_sql = format!(
            "SELECT artist_name, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
             FROM listen_history
             {simple_where} {and_or} artist_name IS NOT NULL
             GROUP BY artist_name ORDER BY plays DESC LIMIT {top_n}",
            and_or = if simple_where.is_empty() {
                "WHERE"
            } else {
                "AND"
            },
        );
        let top_artists: Vec<TopArtistEntry> = self
            .db
            .query_many(&artists_sql, &[])?
            .into_iter()
            .map(|cols| TopArtistEntry {
                artist_name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Top albums ──
        let albums_sql = format!(
            "SELECT h.album_title, h.artist_name, COALESCE(a.cover_path, h.cover_url) as cover_path, COUNT(*) as plays
             FROM listen_history h
             LEFT JOIN albums a ON a.title = h.album_title
             {where_clause} {and_or} h.album_title IS NOT NULL
             GROUP BY h.album_title, h.artist_name, cover_path
             ORDER BY plays DESC LIMIT {top_n}",
            and_or = if where_clause.is_empty() {
                "WHERE"
            } else {
                "AND"
            },
        );
        let top_albums: Vec<TopAlbumEntry> = self
            .db
            .query_many(&albums_sql, &[])?
            .into_iter()
            .map(|cols| TopAlbumEntry {
                album_title: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                artist_name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                cover_path: cols.get(2).and_then(|v| v.as_string()),
                plays: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Top tracks ──
        let tracks_sql = format!(
            "SELECT MAX(track_id), title, artist_name, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
             FROM listen_history
             {simple_where}
             GROUP BY title, artist_name ORDER BY plays DESC LIMIT {top_n}"
        );
        let top_tracks: Vec<TopTrackEntry> = self
            .db
            .query_many(&tracks_sql, &[])?
            .into_iter()
            .map(|cols| TopTrackEntry {
                track_id: cols.first().and_then(|v| v.as_i64()),
                title: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                artist_name: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Trend (daily) ──
        let trend_days = days.unwrap_or(365);
        let trend_zone_and = match zone_id {
            Some(zid) => format!("AND zone_id = {zid}"),
            None => String::new(),
        };
        let trend_sql = format!(
            "SELECT {} as day, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
             FROM listen_history
             WHERE {} {trend_zone_and}
             GROUP BY day ORDER BY day",
            dialect_day("listened_at"),
            dialect_since("listened_at", trend_days)
        );
        let trend: Vec<TrendEntry> = self
            .db
            .query_many(&trend_sql, &[])?
            .into_iter()
            .map(|cols| TrendEntry {
                day: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Hourly distribution ──
        let hourly_sql = format!(
            "SELECT {} as hour, COUNT(*) as plays
             FROM listen_history
             {simple_where}
             GROUP BY hour ORDER BY hour",
            dialect_hour("listened_at")
        );
        let hourly: Vec<HourlyEntry> = self
            .db
            .query_many(&hourly_sql, &[])?
            .into_iter()
            .map(|cols| HourlyEntry {
                hour: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── By zone ──
        let by_zone_sql = format!(
            "SELECT h.zone_id, z.name, COUNT(*) as plays, COALESCE(SUM(h.duration_ms), 0) as ms
             FROM listen_history h
             LEFT JOIN zones z ON z.id = h.zone_id
             {where_clause}
             GROUP BY h.zone_id, z.name ORDER BY plays DESC"
        );
        let by_zone: Vec<ByZoneEntry> = self
            .db
            .query_many(&by_zone_sql, &[])?
            .into_iter()
            .map(|cols| ByZoneEntry {
                zone_id: cols.first().and_then(|v| v.as_i64()),
                zone_name: cols.get(1).and_then(|v| v.as_string()),
                plays: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── By source ──
        let by_source_sql = format!(
            "SELECT source, COUNT(*) as plays, COALESCE(SUM(duration_ms), 0) as ms
             FROM listen_history
             {simple_where}
             GROUP BY source ORDER BY plays DESC"
        );
        let by_source: Vec<BySourceEntry> = self
            .db
            .query_many(&by_source_sql, &[])?
            .into_iter()
            .map(|cols| BySourceEntry {
                source: cols.first().and_then(|v| v.as_string()),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Completion stats ──
        let inner_where = if simple_conditions.is_empty() {
            String::new()
        } else {
            // Re-build the WHERE with `lh.` prefix on column names.
            // simple_conditions hold either a `since_days(listened_at,
            // N)` clause (which already references `listened_at` —
            // need `lh.listened_at`) or `zone_id = N` (need
            // `lh.zone_id`). Easier: just emit the lh-prefixed form
            // directly here rather than try to mangle the strings.
            let mut lh: Vec<String> = Vec::new();
            if let Some(d) = days {
                lh.push(dialect_since("lh.listened_at", d));
            }
            if let Some(zid) = zone_id {
                lh.push(format!("lh.zone_id = {zid}"));
            }
            format!("WHERE {}", lh.join(" AND "))
        };
        let inner_and_or = if simple_conditions.is_empty() {
            "WHERE"
        } else {
            "AND"
        };
        let completion_sql = format!(
            "SELECT
                COUNT(CASE WHEN duration_ms >= 30000 THEN 1 END),
                COUNT(CASE WHEN duration_ms < 30000 THEN 1 END),
                COALESCE(AVG(duration_ms), 0),
                COALESCE((SELECT AVG(t.duration_ms) FROM listen_history lh
                          LEFT JOIN tracks t ON t.id = lh.track_id
                          {inner_where} {inner_and_or} t.duration_ms IS NOT NULL), 0)
             FROM listen_history
             {simple_where}"
        );
        let comp_row = self
            .db
            .query_one(&completion_sql, &[])?
            .ok_or("completion query returned no row")?;
        let completion = CompletionStats {
            completed: comp_row.first().and_then(|v| v.as_i64()).unwrap_or(0),
            skipped: comp_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            avg_listened_ms: comp_row.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64,
            avg_track_duration_ms: comp_row.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64,
        };

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
        source_id: cols.get(6).and_then(|v| v.as_string()),
        album_id: cols.get(7).and_then(|v| v.as_i64()),
        duration_ms: cols.get(8).and_then(|v| v.as_i64()).unwrap_or(0),
        listened_at: cols.get(9).and_then(|v| v.as_string()),
        zone_id: cols.get(10).and_then(|v| v.as_i64()),
        cover_url: None,
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
            source_id: None,
            album_id: None,
            duration_ms: 562_000,
            listened_at: None,
            zone_id: None,
            cover_url: None,
        };

        repo.record(&rec).unwrap();
        repo.record(&rec).unwrap();

        let recent = repo.recent(10).unwrap();
        assert_eq!(recent.len(), 2);

        let top = repo.top_tracks(5).unwrap();
        assert_eq!(top[0]["title"], "So What");
        assert_eq!(top[0]["plays"], 2);

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
                source_id: None,
                album_id: None,
                duration_ms: 300_000,
                listened_at: None,
                zone_id: None,
                cover_url: None,
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
                source_id: None,
                album_id: None,
                duration_ms: 400_000,
                listened_at: None,
                zone_id: None,
                cover_url: None,
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
                source_id: None,
                album_id: None,
                duration_ms: 300_000,
                listened_at: None,
                zone_id: None,
                cover_url: None,
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
            source_id: None,
            album_id: None,
            duration_ms: 0,
            listened_at: None,
            zone_id: None,
            cover_url: None,
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
            source_id: None,
            album_id: None,
            duration_ms: 300_000,
            listened_at: None,
            zone_id: None,
            cover_url: None,
        })
        .unwrap();
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "B".into(),
            artist_name: Some("Y".into()),
            album_title: None,
            source: "tidal".into(),
            source_id: None,
            album_id: None,
            duration_ms: 200_000,
            listened_at: None,
            zone_id: None,
            cover_url: None,
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
                source_id: None,
                album_id: None,
                duration_ms: 0,
                listened_at: None,
                zone_id: None,
                cover_url: None,
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
        assert!(sql::record(&s).contains("VALUES (?, ?, ?, ?, ?, ?, ?, ?)"));
        assert!(sql::record(&p).contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"));
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
            source_id: None,
            album_id: None,
            duration_ms: 100_000,
            listened_at: None,
            zone_id: Some(1),
            cover_url: None,
        })
        .unwrap();

        let recent = repo.recent(1).unwrap();
        assert_eq!(recent[0].zone_id, Some(1));
    }

    #[test]
    fn with_backend_constructor_full_dashboard() {
        // full_dashboard now works through DbBackend — no more
        // sqlite_legacy. Smoke test that it returns a valid struct on
        // a fresh DB (empty results, but no panic / SQL error).
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = HistoryRepo::with_backend(backend);
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: "via-backend".into(),
            artist_name: Some("X".into()),
            album_title: None,
            source: "local".into(),
            source_id: None,
            album_id: None,
            duration_ms: 0,
            listened_at: None,
            zone_id: None,
            cover_url: None,
        })
        .unwrap();
        assert_eq!(repo.count().unwrap(), 1);
        let dash = repo.full_dashboard("7d", None, None, 10).unwrap();
        assert_eq!(dash.totals.plays, 1);
        assert_eq!(dash.totals.unique_artists, 1);
    }
}
