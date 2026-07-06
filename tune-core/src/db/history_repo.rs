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
            "INSERT INTO listen_history (track_id, title, artist_name, album_title, source, source_id, album_id, duration_ms, zone_id, cover_url, profile_id) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11)
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
             WHERE h.source != 'radio' \
             GROUP BY h.title, h.artist_name \
             ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn top_artists<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT artist_name, COUNT(*) as plays FROM listen_history WHERE artist_name IS NOT NULL AND source != 'radio' GROUP BY artist_name ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn top_albums<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT album_title, artist_name, COUNT(*) as plays FROM listen_history WHERE album_title IS NOT NULL AND source != 'radio' GROUP BY album_title, artist_name ORDER BY plays DESC LIMIT {}",
            d.placeholder(1)
        )
    }

    pub fn dashboard_total_duration() -> &'static str {
        "SELECT CAST(COALESCE(SUM(duration_ms), 0) AS BIGINT) FROM listen_history WHERE source != 'radio'"
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
            "SELECT {day_col} as day, COUNT(*) as play_count, CAST(COALESCE(SUM(duration_ms), 0) AS BIGINT) as total_ms \
             FROM listen_history WHERE {since} \
             GROUP BY 1 ORDER BY 1"
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
    pub profile_id: Option<i64>,
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
        let params: [&dyn ToSqlValue; 11] = [
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
            &rec.profile_id,
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

    pub fn clear(&self) -> Result<(), String> {
        self.db.execute("DELETE FROM listen_history", &[])?;
        Ok(())
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
                    CAST(COALESCE(SUM(duration_ms), 0) AS BIGINT),
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

        // ── Top artists (exclude radio) ──
        let no_radio_and = if simple_where.is_empty() {
            "WHERE source != 'radio' AND"
        } else {
            "AND source != 'radio' AND"
        };
        let artists_sql = format!(
            "SELECT lh.artist_name, COUNT(*) as plays, CAST(COALESCE(SUM(lh.duration_ms), 0) AS BIGINT) as ms,
                    COALESCE(ar.image_path, (
                        SELECT a2.cover_path FROM albums a2
                        JOIN tracks t2 ON t2.album_id = a2.id
                        WHERE LOWER(t2.album_artist) = LOWER(lh.artist_name) AND a2.cover_path IS NOT NULL
                        LIMIT 1
                    )) as cover_path
             FROM listen_history lh
             LEFT JOIN artists ar ON LOWER(ar.name) = LOWER(lh.artist_name)
             {simple_where} {no_radio_and} lh.artist_name IS NOT NULL
             GROUP BY lh.artist_name, ar.image_path ORDER BY plays DESC LIMIT {top_n}",
        );
        let top_artists: Vec<TopArtistEntry> = self
            .db
            .query_many(&artists_sql, &[])?
            .into_iter()
            .map(|cols| TopArtistEntry {
                artist_name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                cover_path: cols.get(3).and_then(|v| v.as_string()),
            })
            .collect();

        // ── Top albums (exclude radio) ──
        let no_radio_and_h = if where_clause.is_empty() {
            "WHERE h.source != 'radio' AND"
        } else {
            "AND h.source != 'radio' AND"
        };
        let albums_sql = format!(
            "SELECT h.album_title, h.artist_name, COALESCE(a.cover_path, h.cover_url) as cover_path, COUNT(*) as plays, MAX(a.id) as album_id,
                    MAX(h.source) as source, MAX(h.source_id) as source_id
             FROM listen_history h
             LEFT JOIN albums a ON LOWER(a.title) = LOWER(h.album_title)
             {where_clause} {no_radio_and_h} h.album_title IS NOT NULL
             GROUP BY h.album_title, h.artist_name, COALESCE(a.cover_path, h.cover_url)
             ORDER BY plays DESC LIMIT {top_n}",
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
                album_id: cols.get(4).and_then(|v| v.as_i64()),
                source: cols.get(5).and_then(|v| v.as_string()),
                source_id: cols.get(6).and_then(|v| v.as_string()),
            })
            .collect();

        // ── Top tracks (exclude radio) ──
        let tracks_sql = format!(
            "SELECT COALESCE(MAX(lh.track_id), (
                        SELECT t3.id FROM tracks t3
                        WHERE LOWER(t3.title) = LOWER(lh.title)
                          AND LOWER(COALESCE(t3.album_artist, '')) = LOWER(COALESCE(lh.artist_name, ''))
                        LIMIT 1
                    )) as track_id,
                    lh.title, lh.artist_name, COUNT(*) as plays,
                    CAST(COALESCE(SUM(lh.duration_ms), 0) AS BIGINT) as ms,
                    COALESCE(MAX(lh.cover_url), (
                        SELECT a2.cover_path FROM tracks t2
                        JOIN albums a2 ON t2.album_id = a2.id
                        WHERE LOWER(t2.title) = LOWER(lh.title)
                          AND LOWER(COALESCE(t2.album_artist, '')) = LOWER(COALESCE(lh.artist_name, ''))
                          AND a2.cover_path IS NOT NULL
                        LIMIT 1
                    )) as cover_path,
                    MAX(lh.source) as source, MAX(lh.source_id) as source_id
             FROM listen_history lh
             {simple_where} {no_radio_and} lh.title IS NOT NULL
             GROUP BY lh.title, lh.artist_name ORDER BY plays DESC LIMIT {top_n}"
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
                cover_path: cols.get(5).and_then(|v| v.as_string()),
                source: cols.get(6).and_then(|v| v.as_string()),
                source_id: cols.get(7).and_then(|v| v.as_string()),
            })
            .collect();

        // ── Top radios ──
        // Radio plays are NOT in listen_history (deliberately — see the
        // record_history guard in the orchestrator: a frozen station title
        // produces bogus rows on every replay). They live in radio_stations
        // (record_play bumps play_count), so the top list comes from there.
        // play_count is lifetime, so this ignores the period filter (radio
        // listening is sparse; lifetime top is the useful view).
        // CASTs keep this working on both backends: on Postgres radio_stations
        // has drifted to play_count TEXT (pg_migrate) and a bare `NULL as
        // cover_path` (untyped) makes sqlx fail to decode — either made the whole
        // query error and return empty (Top Radios blank on the .15 PG server).
        // Explicit integer/text types are identity on SQLite.
        let radios_sql = format!(
            "SELECT name as station_name,
                    CAST(play_count AS INTEGER) as plays,
                    CAST(0 AS BIGINT) as ms,
                    logo_url as cover_url,
                    id as radio_id,
                    CAST(NULL AS TEXT) as cover_path
             FROM radio_stations
             WHERE CAST(play_count AS INTEGER) > 0
             ORDER BY CAST(play_count AS INTEGER) DESC LIMIT {top_n}"
        );
        let top_radios: Vec<TopRadioEntry> = self
            .db
            .query_many(&radios_sql, &[])
            .unwrap_or_default()
            .into_iter()
            .map(|cols| TopRadioEntry {
                station_name: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
                cover_url: cols.get(3).and_then(|v| v.as_string()),
                radio_id: cols.get(4).and_then(|v| v.as_i64()),
                cover_path: cols.get(5).and_then(|v| v.as_string()),
            })
            .collect();

        // ── Trend (daily) ──
        let trend_days = days.unwrap_or(365);
        let trend_zone_and = match zone_id {
            Some(zid) => format!("AND zone_id = {zid}"),
            None => String::new(),
        };
        let trend_sql = format!(
            "SELECT {} as day, COUNT(*) as plays, CAST(COALESCE(SUM(duration_ms), 0) AS BIGINT) as ms
             FROM listen_history
             WHERE {} {trend_zone_and}
             GROUP BY 1 ORDER BY 1",
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
             GROUP BY 1 ORDER BY 1",
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
            "SELECT h.zone_id, z.name, COUNT(*) as plays, CAST(COALESCE(SUM(h.duration_ms), 0) AS BIGINT) as ms
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
            "SELECT source, COUNT(*) as plays, CAST(COALESCE(SUM(duration_ms), 0) AS BIGINT) as ms
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
        let cast_dur = match self.db.engine() {
            Engine::Sqlite => "t.duration_ms".to_string(),
            Engine::Postgres => "CAST(t.duration_ms AS bigint)".to_string(),
        };
        let avg_cast = match self.db.engine() {
            Engine::Sqlite => ("AVG(duration_ms)".to_string(), format!("AVG({cast_dur})")),
            Engine::Postgres => (
                "AVG(duration_ms)::float8".to_string(),
                format!("AVG({cast_dur})::float8"),
            ),
        };
        let completion_sql = format!(
            "SELECT
                COUNT(CASE WHEN duration_ms >= 30000 THEN 1 END),
                COUNT(CASE WHEN duration_ms < 30000 THEN 1 END),
                COALESCE({avg0}, 0),
                COALESCE((SELECT {avg1} FROM listen_history lh
                          LEFT JOIN tracks t ON t.id = lh.track_id
                          {inner_where} {inner_and_or} {cast_dur} IS NOT NULL), 0)
             FROM listen_history
             {simple_where}",
            avg0 = avg_cast.0,
            avg1 = avg_cast.1,
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

        // ── By genre (via tracks join) ──
        let genre_sql = format!(
            "SELECT t.genre, COUNT(*) as plays, CAST(COALESCE(SUM(h.duration_ms), 0) AS BIGINT) as ms
             FROM listen_history h
             INNER JOIN tracks t ON t.id = h.track_id
             {where_clause} {and_or} t.genre IS NOT NULL AND t.genre != ''
             GROUP BY t.genre ORDER BY plays DESC LIMIT 50",
            and_or = if where_clause.is_empty() {
                "WHERE"
            } else {
                "AND"
            },
        );
        let by_genre: Vec<ByGenreEntry> = self
            .db
            .query_many(&genre_sql, &[])
            .unwrap_or_default()
            .into_iter()
            .map(|cols| ByGenreEntry {
                genre: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                plays: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                listening_ms: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Weekday × hour heatmap ──
        let extract_dow = match self.db.engine() {
            Engine::Sqlite => "CAST(strftime('%w', listened_at) AS INTEGER)".to_string(),
            Engine::Postgres => "EXTRACT(DOW FROM listened_at::timestamp)::int".to_string(),
        };
        let wh_sql = format!(
            "SELECT CASE WHEN {extract_dow} = 0 THEN 7 ELSE {extract_dow} END as wd,
                    {hour_expr} as hr, COUNT(*) as plays
             FROM listen_history
             {simple_where}
             GROUP BY 1, 2 ORDER BY 1, 2",
            hour_expr = dialect_hour("listened_at"),
        );
        let weekday_hourly: Vec<WeekdayHourlyEntry> = self
            .db
            .query_many(&wh_sql, &[])
            .unwrap_or_default()
            .into_iter()
            .map(|cols| WeekdayHourlyEntry {
                weekday: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
                hour: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                plays: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        // ── Streak ──
        let streak_day_expr = dialect_day("listened_at");
        let streak_sql =
            format!("SELECT DISTINCT {streak_day_expr} as d FROM listen_history ORDER BY 1");
        let all_days: Vec<String> = self
            .db
            .query_many(&streak_sql, &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_string()))
            .collect();
        let streak = if !all_days.is_empty() {
            let today = to.split('T').next().unwrap_or("");
            let mut best = 1i64;
            let mut current = 1i64;
            for w in all_days.windows(2) {
                if is_consecutive_days_str(&w[0], &w[1]) {
                    current += 1;
                } else {
                    best = best.max(current);
                    current = 1;
                }
            }
            best = best.max(current);
            let last_day = all_days.last().cloned();
            let current_streak = if last_day.as_deref() == Some(today) {
                current
            } else {
                0
            };
            Some(StreakInfo {
                current: current_streak,
                best,
                last_day,
            })
        } else {
            None
        };

        // ── On this day ──
        let today_md = to.get(5..10).unwrap_or("01-01");
        let otd_like = format!("%-{today_md}%");
        let ph = match self.db.engine() {
            Engine::Sqlite => "?".to_string(),
            Engine::Postgres => "$1".to_string(),
        };
        let current_year = to.get(0..4).unwrap_or("2026");
        let yr_expr = match self.db.engine() {
            Engine::Sqlite => "CAST(strftime('%Y', listened_at) AS INTEGER)",
            Engine::Postgres => "EXTRACT(YEAR FROM listened_at::timestamp)::int",
        };
        let otd_sql = format!(
            "SELECT title, artist_name, album_title, NULL, listened_at,
                    {yr_expr} as yr
             FROM listen_history
             WHERE listened_at LIKE {ph} AND {yr_expr} < {current_year}
             GROUP BY title, artist_name
             ORDER BY yr DESC LIMIT 10"
        );
        let on_this_day: Vec<OnThisDayEntry> = self
            .db
            .query_many(
                &otd_sql,
                &[&otd_like as &dyn crate::db::backend::ToSqlValue],
            )
            .unwrap_or_default()
            .into_iter()
            .map(|cols| OnThisDayEntry {
                track_title: cols.first().and_then(|v| v.as_string()),
                artist_name: cols.get(1).and_then(|v| v.as_string()),
                album_title: cols.get(2).and_then(|v| v.as_string()),
                cover_path: cols.get(3).and_then(|v| v.as_string()),
                played_at: cols.get(4).and_then(|v| v.as_string()),
                year: cols.get(5).and_then(|v| v.as_i64()),
            })
            .collect();

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
            top_radios,
            trend,
            hourly,
            by_zone,
            by_source,
            completion,
            by_genre,
            weekday_hourly,
            streak,
            on_this_day,
        })
    }
}

fn is_consecutive_days_str(a: &str, b: &str) -> bool {
    fn to_days(s: &str) -> Option<i64> {
        let s = s.split('T').next().unwrap_or(s);
        let parts: Vec<&str> = s.split('-').collect();
        if parts.len() != 3 {
            return None;
        }
        let y: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let d: i64 = parts[2].parse().ok()?;
        Some(y * 366 + m * 31 + d)
    }
    match (to_days(a), to_days(b)) {
        (Some(da), Some(db)) => db - da == 1,
        _ => false,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopAlbumEntry {
    pub album_id: Option<i64>,
    pub album_title: String,
    pub artist_name: String,
    pub cover_path: Option<String>,
    pub plays: i64,
    /// Streaming service ("qobuz"/"tidal"/"youtube"/…) or "local". Lets the UI
    /// play a streaming top item that has no local album_id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopTrackEntry {
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: String,
    pub plays: i64,
    pub listening_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
    /// Streaming service ("qobuz"/"tidal"/"youtube"/…) or "local". Lets the UI
    /// play a streaming top item that has no local track_id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopRadioEntry {
    pub station_name: String,
    pub radio_id: Option<i64>,
    pub plays: i64,
    pub listening_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_path: Option<String>,
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
pub struct ByGenreEntry {
    pub genre: String,
    pub plays: i64,
    pub listening_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeekdayHourlyEntry {
    pub weekday: i64,
    pub hour: i64,
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreakInfo {
    pub current: i64,
    pub best: i64,
    pub last_day: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnThisDayEntry {
    pub track_title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_path: Option<String>,
    pub played_at: Option<String>,
    pub year: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardData {
    pub period: String,
    pub range: DashboardRange,
    pub totals: DashboardTotals,
    pub top_artists: Vec<TopArtistEntry>,
    pub top_albums: Vec<TopAlbumEntry>,
    pub top_tracks: Vec<TopTrackEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub top_radios: Vec<TopRadioEntry>,
    pub trend: Vec<TrendEntry>,
    pub hourly: Vec<HourlyEntry>,
    pub by_zone: Vec<ByZoneEntry>,
    pub by_source: Vec<BySourceEntry>,
    pub completion: CompletionStats,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_genre: Vec<ByGenreEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub weekday_hourly: Vec<WeekdayHourlyEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streak: Option<StreakInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub on_this_day: Vec<OnThisDayEntry>,
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
        profile_id: None,
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
            profile_id: None,
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
                profile_id: None,
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
                profile_id: None,
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
                profile_id: None,
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
            profile_id: None,
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
            profile_id: None,
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
            profile_id: None,
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
                profile_id: None,
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
        assert!(sql::record(&s).contains("VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"));
        assert!(sql::record(&p).contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
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
            profile_id: None,
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
            profile_id: None,
        })
        .unwrap();
        assert_eq!(repo.count().unwrap(), 1);
        let dash = repo.full_dashboard("7d", None, None, 10).unwrap();
        assert_eq!(dash.totals.plays, 1);
        assert_eq!(dash.totals.unique_artists, 1);
    }
}
