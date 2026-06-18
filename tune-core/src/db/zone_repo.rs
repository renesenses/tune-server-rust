use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for zone_repo.
pub mod sql {
    use super::SqlDialect;

    // NOTE: autoplay_enabled intentionally omitted from COLS.
    // The column is added by migration v36, but on Windows the migration
    // can fail silently (file locking).  COALESCE(autoplay_enabled, 1)
    // still crashes with "no such column" when the column doesn't exist.
    // row_to_zone reads cols.get(16) → None → defaults to true, which is
    // the correct fallback.  The separate get_autoplay_enabled() method
    // handles reading the actual value safely.
    const COLS: &str = "id, name, output_type, output_device_id, volume, muted, online, gapless_enabled, group_id, sync_delay_ms, last_position_ms, last_track_id, last_track_source, last_track_source_id, max_sample_rate, fixed_volume";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("SELECT {COLS} FROM zones WHERE id = {}", d.placeholder(1))
    }

    pub fn get_by_device_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM zones WHERE output_device_id = {}",
            d.placeholder(1)
        )
    }

    pub fn list_all() -> String {
        format!("SELECT {COLS} FROM zones ORDER BY name")
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO zones (name, output_type, output_device_id) VALUES ({}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    /// Delete duplicate zones, keeping only the one with the lowest id for each
    /// output_device_id. Returns the DELETE statement.
    pub fn deduplicate() -> &'static str {
        "DELETE FROM zones WHERE id NOT IN (SELECT MIN(id) FROM zones WHERE output_device_id IS NOT NULL GROUP BY output_device_id) AND output_device_id IS NOT NULL AND output_device_id IN (SELECT output_device_id FROM zones WHERE output_device_id IS NOT NULL GROUP BY output_device_id HAVING COUNT(*) > 1)"
    }

    pub fn update_field<D: SqlDialect>(d: &D, field: &str) -> String {
        format!(
            "UPDATE zones SET {field} = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_online_by_device<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET online = {} WHERE output_device_id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_by_id<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM zones WHERE id = {}", d.placeholder(1))
    }

    pub fn save_playback_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET last_position_ms = {}, last_track_id = {}, last_track_source = {}, last_track_source_id = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5)
        )
    }

    pub fn clear_playback_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET last_position_ms = 0, last_track_id = NULL, last_track_source = NULL, last_track_source_id = NULL WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn update_dsp<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE zones SET dsp_preset_id = {}, dsp_enabled = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn get_dsp_config<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT dsp_preset_id, COALESCE(dsp_enabled, 0) FROM zones WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM zones"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: Option<i64>,
    pub name: String,
    pub output_type: Option<String>,
    pub output_device_id: Option<String>,
    pub volume: i32,
    pub muted: bool,
    pub online: bool,
    pub gapless_enabled: bool,
    pub group_id: Option<String>,
    pub sync_delay_ms: i32,
    pub last_position_ms: i64,
    pub last_track_id: Option<i64>,
    pub last_track_source: Option<String>,
    pub last_track_source_id: Option<String>,
    pub max_sample_rate: Option<u32>,
    pub fixed_volume: bool,
    pub autoplay_enabled: bool,
}

pub struct ZoneRepo {
    db: Arc<dyn DbBackend>,
}

impl ZoneRepo {
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

    fn update_field_sql(&self, field: &str) -> String {
        match self.db.engine() {
            Engine::Sqlite => sql::update_field(&SqliteDialect, field),
            Engine::Postgres => sql::update_field(&PostgresDialect, field),
        }
    }

    pub fn get(&self, id: i64) -> Result<Option<Zone>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_zone))
    }

    /// Look up a zone by its output device id.
    pub fn get_by_device_id(&self, device_id: &str) -> Result<Option<Zone>, String> {
        let sql = self.dialect_sql(sql::get_by_device_id, sql::get_by_device_id);
        let params: [&dyn ToSqlValue; 1] = [&device_id];
        // Try read path first, fall back to strong (same pattern as list).
        if let Some(row) = self.db.query_one(&sql, &params)? {
            return Ok(Some(row_to_zone(&row)));
        }
        // Strong read to see the writer's own pending commits (WAL lag).
        let rows = self.db.query_many_strong(&sql, &params)?;
        Ok(rows.first().map(row_to_zone))
    }

    pub fn list(&self) -> Result<Vec<Zone>, String> {
        // First try the read path (cheap, no contention with writes).
        // If empty, fall back to query_many_strong — under WAL the
        // read-only snapshot can lag behind the write conn's commits,
        // which surfaced as the "zone disappears after create" P0
        // (forum #2, #6). The strong path always sees the writer's
        // own commits. Pattern preserved from commit 8af95ec.
        let query = sql::list_all();
        let rows = self.db.query_many(&query, &[])?;
        if !rows.is_empty() {
            return Ok(rows.iter().map(row_to_zone).collect());
        }
        let strong = self.db.query_many_strong(&query, &[])?;
        Ok(strong.iter().map(row_to_zone).collect())
    }

    pub fn create(
        &self,
        name: &str,
        output_type: Option<&str>,
        output_device_id: Option<&str>,
    ) -> Result<i64, String> {
        // INSERT + last_insert_rowid. We deliberately do NOT use
        // write_tx here: a write tx wraps in `BEGIN DEFERRED`, which
        // fails when a SQLite-level transaction is already in progress
        // (cf. the `create_zone_during_open_transaction` test, where
        // a scan tx is active). Sequential `execute` + `last_insert_rowid`
        // each take the write lock briefly and don't try to start a
        // new tx; both calls share the same rusqlite mutex on SQLite so
        // the rowid we read reflects the INSERT we just did.
        let create_sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 3] = [&name, &output_type, &output_device_id];
        self.db.execute(&create_sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    /// Atomically get an existing zone by output_device_id, or create a new one.
    /// Returns `(zone_id, created)` where `created` is true if a new zone was inserted.
    ///
    /// If a concurrent writer inserts the same device_id between our check and
    /// our INSERT (race), the UNIQUE index will reject the INSERT.  We catch
    /// that and return the existing zone instead of propagating the error.
    pub fn get_or_create(
        &self,
        name: &str,
        output_type: Option<&str>,
        output_device_id: &str,
    ) -> Result<(i64, bool), String> {
        // Check if a zone with this device_id already exists.
        if let Some(existing) = self.get_by_device_id(output_device_id)? {
            if let Some(id) = existing.id {
                return Ok((id, false));
            }
        }
        // No existing zone — try to create one.
        match self.create(name, output_type, Some(output_device_id)) {
            Ok(id) => Ok((id, true)),
            Err(e) if e.contains("UNIQUE constraint failed") => {
                // Race: another thread inserted the same device_id between our
                // check and our INSERT.  Return the existing zone.
                if let Some(existing) = self.get_by_device_id(output_device_id)? {
                    if let Some(id) = existing.id {
                        return Ok((id, false));
                    }
                }
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Remove duplicate zones that share the same output_device_id, keeping only
    /// the one with the lowest id. Returns the number of duplicates removed.
    pub fn deduplicate(&self) -> Result<usize, String> {
        self.db.execute(sql::deduplicate(), &[])
    }

    pub fn update_volume(&self, id: i64, volume: i32) -> Result<(), String> {
        let sql = self.update_field_sql("volume");
        let params: [&dyn ToSqlValue; 2] = [&volume, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_muted(&self, id: i64, muted: bool) -> Result<(), String> {
        let val: String = if muted { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("muted");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_name(&self, id: i64, name: &str) -> Result<(), String> {
        let sql = self.update_field_sql("name");
        let params: [&dyn ToSqlValue; 2] = [&name, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_output_device(&self, id: i64, device_id: &str) -> Result<(), String> {
        let sql = self.update_field_sql("output_device_id");
        let params: [&dyn ToSqlValue; 2] = [&device_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_output_type(&self, id: i64, output_type: &str) -> Result<(), String> {
        let sql = self.update_field_sql("output_type");
        let params: [&dyn ToSqlValue; 2] = [&output_type, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_online(&self, id: i64, online: bool) -> Result<(), String> {
        let val: String = if online { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("online");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_gapless_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("gapless_enabled");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_fixed_volume(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("fixed_volume");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_autoplay_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.update_field_sql("autoplay_enabled");
        let params: [&dyn ToSqlValue; 2] = [&val, &id];
        // Column may not exist on pre-v36 databases (Windows migration
        // failure).  Swallow the error — the feature degrades gracefully
        // to always-enabled.
        match self.db.execute(&sql, &params) {
            Ok(_) => Ok(()),
            Err(e) if e.contains("no such column") || e.contains("does not exist") => {
                tracing::debug!(id, error = %e, "autoplay_enabled_column_missing_ignoring_update");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Safely read autoplay_enabled for a zone.  Returns true (the default)
    /// if the column doesn't exist (pre-v36 database).
    pub fn get_autoplay_enabled(&self, id: i64) -> bool {
        // Try reading the column directly.  If the column doesn't exist,
        // the query fails and we return the default (true).
        let placeholder = match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(1),
            Engine::Postgres => PostgresDialect.placeholder(1),
        };
        let sql =
            format!("SELECT COALESCE(autoplay_enabled, 1) FROM zones WHERE id = {placeholder}");
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db
            .query_one(&sql, &params)
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .map(|v| v != 0)
            .unwrap_or(true)
    }

    pub fn set_online_by_device(&self, device_id: &str, online: bool) -> Result<usize, String> {
        let val: String = if online { "1".into() } else { "0".into() };
        let sql = self.dialect_sql(sql::set_online_by_device, sql::set_online_by_device);
        let params: [&dyn ToSqlValue; 2] = [&val, &device_id];
        self.db.execute(&sql, &params)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_id, sql::delete_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_group(&self, id: i64, group_id: Option<&str>) -> Result<(), String> {
        let sql = self.update_field_sql("group_id");
        let params: [&dyn ToSqlValue; 2] = [&group_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_sync_delay(&self, id: i64, ms: i32) -> Result<(), String> {
        let sql = self.update_field_sql("sync_delay_ms");
        let params: [&dyn ToSqlValue; 2] = [&ms, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_max_sample_rate(&self, id: i64, rate: Option<u32>) -> Result<(), String> {
        let sql = self.update_field_sql("max_sample_rate");
        let rate_i64 = rate.map(|r| r as i64);
        let params: [&dyn ToSqlValue; 2] = [&rate_i64, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn save_playback_position(
        &self,
        id: i64,
        position_ms: i64,
        track_id: Option<i64>,
        source: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::save_playback_position, sql::save_playback_position);
        let params: [&dyn ToSqlValue; 5] = [&position_ms, &track_id, &source, &source_id, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn clear_playback_position(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::clear_playback_position, sql::clear_playback_position);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn update_dsp(&self, id: i64, preset_id: Option<i64>, enabled: bool) -> Result<(), String> {
        let preset_str: Option<String> = preset_id.map(|v| v.to_string());
        let en: String = if enabled { "1".into() } else { "0".into() };
        let sql = self.dialect_sql(sql::update_dsp, sql::update_dsp);
        let params: [&dyn ToSqlValue; 3] = [&preset_str, &en, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn get_dsp_config(&self, id: i64) -> Result<(Option<i64>, bool), String> {
        let sql = self.dialect_sql(sql::get_dsp_config, sql::get_dsp_config);
        let params: [&dyn ToSqlValue; 1] = [&id];
        let row = self
            .db
            .query_one(&sql, &params)?
            .ok_or_else(|| format!("zone {id} not found"))?;
        let preset = row.first().and_then(|v| v.as_i64());
        let enabled = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0) != 0;
        Ok((preset, enabled))
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }
}

fn row_to_zone(cols: &Vec<SqlValue>) -> Zone {
    Zone {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        output_type: cols.get(2).and_then(|v| v.as_string()),
        output_device_id: cols.get(3).and_then(|v| v.as_string()),
        volume: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(50) as i32,
        muted: cols.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        online: cols.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        gapless_enabled: cols.get(7).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        group_id: cols.get(8).and_then(|v| v.as_string()),
        sync_delay_ms: cols.get(9).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        last_position_ms: cols.get(10).and_then(|v| v.as_i64()).unwrap_or(0),
        last_track_id: cols.get(11).and_then(|v| v.as_i64()),
        last_track_source: cols.get(12).and_then(|v| v.as_string()),
        last_track_source_id: cols.get(13).and_then(|v| v.as_string()),
        max_sample_rate: cols.get(14).and_then(|v| v.as_i64()).map(|v| v as u32),
        fixed_volume: cols.get(15).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        autoplay_enabled: cols.get(16).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn crud_zone() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo
            .create("Living Room", Some("dlna"), Some("uuid:123"))
            .unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.name, "Living Room");
        assert_eq!(zone.volume, 50);
        assert!(!zone.muted);

        repo.update_volume(id, 75).unwrap();
        repo.update_muted(id, true).unwrap();
        let updated = repo.get(id).unwrap().unwrap();
        assert_eq!(updated.volume, 75);
        assert!(updated.muted);

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn list_zones() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Zone A", None, None).unwrap();
        repo.create("Zone B", None, None).unwrap();
        let zones = repo.list().unwrap();
        assert_eq!(zones.len(), 2);
    }

    #[test]
    fn zone_count() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        assert_eq!(repo.count().unwrap(), 0);
        repo.create("Zone A", None, None).unwrap();
        repo.create("Zone B", None, None).unwrap();
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn zone_update_name() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Old Name", None, None).unwrap();
        repo.update_name(id, "New Name").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.name, "New Name");
    }

    #[test]
    fn zone_update_output_device() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", Some("dlna"), Some("uuid:old")).unwrap();
        repo.update_output_device(id, "uuid:new-device").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.output_device_id.as_deref(), Some("uuid:new-device"));
    }

    #[test]
    fn zone_update_output_type() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", Some("local"), None).unwrap();
        repo.update_output_type(id, "dlna").unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.output_type.as_deref(), Some("dlna"));
    }

    #[test]
    fn zone_default_values() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Default Zone", None, None).unwrap();
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.volume, 50);
        assert!(!zone.muted);
        assert!(zone.online);
        assert!(zone.output_type.is_none());
        assert!(zone.output_device_id.is_none());
    }

    #[test]
    fn zone_mute_unmute() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", None, None).unwrap();
        assert!(!repo.get(id).unwrap().unwrap().muted);

        repo.update_muted(id, true).unwrap();
        assert!(repo.get(id).unwrap().unwrap().muted);

        repo.update_muted(id, false).unwrap();
        assert!(!repo.get(id).unwrap().unwrap().muted);
    }

    #[test]
    fn zone_volume_range() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let id = repo.create("Zone", None, None).unwrap();

        repo.update_volume(id, 0).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().volume, 0);

        repo.update_volume(id, 100).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().volume, 100);
    }

    #[test]
    fn zone_get_nonexistent() {
        let db = test_db();
        let repo = ZoneRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn zone_list_sorted() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Salon", None, None).unwrap();
        repo.create("Bureau", None, None).unwrap();
        repo.create("Chambre", None, None).unwrap();

        let zones = repo.list().unwrap();
        assert_eq!(zones[0].name, "Bureau");
        assert_eq!(zones[1].name, "Chambre");
        assert_eq!(zones[2].name, "Salon");
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create(&s).contains("VALUES (?, ?, ?)"));
        assert!(sql::create(&p).contains("VALUES ($1, $2, $3)"));
        assert!(sql::update_field(&s, "volume").ends_with("SET volume = ? WHERE id = ?"));
        assert!(sql::update_field(&p, "volume").ends_with("SET volume = $1 WHERE id = $2"));
    }

    #[test]
    fn create_zone_during_open_transaction() {
        // Regression test for forum P0 #2 (Dimitri) and #6 (Dominique):
        // a zone created during an open scan tx flashes green then
        // disappears because list() used the read-only snapshot that
        // pre-dated the commit.
        //
        // With the port to query_many_strong as the fallback path,
        // list() now sees the writer's own pending writes — same
        // observable behavior as the original 8af95ec fix.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = SqliteDb::open(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        // Simulate the scan starting a transaction on the write conn.
        db.execute_batch("BEGIN IMMEDIATE").unwrap();

        let repo = ZoneRepo::new(db.clone());
        let id = repo
            .create("Living Room", Some("dlna"), Some("uuid:123"))
            .unwrap();
        assert!(id > 0);

        let zones_before_commit = repo.list().unwrap();
        assert_eq!(zones_before_commit.len(), 1);
        assert_eq!(zones_before_commit[0].name, "Living Room");

        db.execute_batch("COMMIT").unwrap();

        let zones_after_commit = repo.list().unwrap();
        assert_eq!(zones_after_commit.len(), 1);
        assert_eq!(zones_after_commit[0].name, "Living Room");
    }

    #[test]
    fn get_or_create_idempotent() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        let (id1, created1) = repo
            .get_or_create("Living Room", Some("dlna"), "uuid:123")
            .unwrap();
        assert!(created1);

        let (id2, created2) = repo
            .get_or_create("Living Room", Some("dlna"), "uuid:123")
            .unwrap();
        assert!(!created2);
        assert_eq!(id1, id2);

        // Only 1 zone should exist
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn get_by_device_id() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        repo.create("Zone A", Some("dlna"), Some("uuid:aaa"))
            .unwrap();
        repo.create("Zone B", Some("dlna"), Some("uuid:bbb"))
            .unwrap();

        let found = repo.get_by_device_id("uuid:aaa").unwrap().unwrap();
        assert_eq!(found.name, "Zone A");

        let found_b = repo.get_by_device_id("uuid:bbb").unwrap().unwrap();
        assert_eq!(found_b.name, "Zone B");

        assert!(repo.get_by_device_id("uuid:nonexistent").unwrap().is_none());
    }

    #[test]
    fn deduplicate_removes_extra_zones() {
        let db = test_db();
        let repo = ZoneRepo::new(db);

        // Simulate the bug: 3 zones with the same device_id
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        repo.create("Zone A", Some("dlna"), Some("uuid:123"))
            .unwrap();
        // Plus a unique zone
        repo.create("Zone B", Some("dlna"), Some("uuid:456"))
            .unwrap();
        // Plus a zone with no device (manual zone)
        repo.create("Zone C", None, None).unwrap();

        assert_eq!(repo.count().unwrap(), 5);

        let removed = repo.deduplicate().unwrap();
        assert_eq!(removed, 2); // 2 duplicate uuid:123 entries removed

        assert_eq!(repo.count().unwrap(), 3); // 1 uuid:123 + 1 uuid:456 + 1 no-device

        // The remaining uuid:123 zone should be the one with lowest id
        let zones = repo.list().unwrap();
        let z123: Vec<_> = zones
            .iter()
            .filter(|z| z.output_device_id.as_deref() == Some("uuid:123"))
            .collect();
        assert_eq!(z123.len(), 1);
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(backend);
        let id = repo.create("X", None, None).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().name, "X");
    }
}
