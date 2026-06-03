use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::engine::SqlDialect;
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for zone_repo.
pub mod sql {
    use super::SqlDialect;

    const COLS: &str = "id, name, output_type, output_device_id, volume, muted, online, gapless_enabled, group_id, sync_delay_ms, last_position_ms, last_track_id, last_track_source, last_track_source_id";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!("SELECT {COLS} FROM zones WHERE id = {}", d.placeholder(1))
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
}

pub struct ZoneRepo {
    db: SqliteDb,
}

impl ZoneRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Zone>, String> {
        let conn = self.db.read_connection().lock().unwrap();
        let mut stmt = conn
            .prepare(&sql::get_by_id(&self.db.dialect()))
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_zone(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<Zone>, String> {
        // First try the read connection (cheap, no contention with writes).
        let query = sql::list_all();
        let read_zones: Vec<Zone> = {
            let conn = self.db.read_connection().lock().unwrap();
            let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
            stmt.query_map([], |row| Ok(row_to_zone(row)))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };

        // WAL visibility: the read-only connection takes a snapshot at
        // open time and may not see commits from the write connection
        // until the snapshot is released. For a 0-result list — which
        // is the symptom users saw on the "zone disappears after
        // create" P0 (#2, #6) — re-query through the write connection
        // which always sees its own commits.
        // Same pattern as 8af95ec on play_queue/queue.
        if !read_zones.is_empty() {
            return Ok(read_zones);
        }
        let wconn = self.db.connection().lock().unwrap();
        let mut wstmt = wconn.prepare(&query).map_err(|e| e.to_string())?;
        let write_zones = wstmt
            .query_map([], |row| Ok(row_to_zone(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(write_zones)
    }

    pub fn create(
        &self,
        name: &str,
        output_type: Option<&str>,
        output_device_id: Option<&str>,
    ) -> Result<i64, String> {
        // Use a single connection lock so INSERT + last_insert_rowid are atomic.
        let create_sql = sql::create(&self.db.dialect());
        self.db.write(|conn| {
            conn.execute(
                &create_sql,
                rusqlite::params![name, output_type, output_device_id],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn update_volume(&self, id: i64, volume: i32) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "volume"),
            &[&volume as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_muted(&self, id: i64, muted: bool) -> Result<(), String> {
        let val = if muted { 1i64 } else { 0i64 };
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "muted"),
            &[&val as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_name(&self, id: i64, name: &str) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "name"),
            &[&name as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_output_device(&self, id: i64, device_id: &str) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "output_device_id"),
            &[&device_id as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_output_type(&self, id: i64, output_type: &str) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "output_type"),
            &[&output_type as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_online(&self, id: i64, online: bool) -> Result<(), String> {
        let val = if online { 1i64 } else { 0i64 };
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "online"),
            &[&val as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_gapless_enabled(&self, id: i64, enabled: bool) -> Result<(), String> {
        let val = if enabled { 1i64 } else { 0i64 };
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "gapless_enabled"),
            &[&val as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn set_online_by_device(&self, device_id: &str, online: bool) -> Result<usize, String> {
        let val = if online { 1i64 } else { 0i64 };
        self.db.execute(
            &sql::set_online_by_device(&self.db.dialect()),
            &[&val as &dyn rusqlite::types::ToSql, &device_id],
        )
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db
            .execute(&sql::delete_by_id(&self.db.dialect()), &[&id])?;
        Ok(())
    }

    pub fn update_group(&self, id: i64, group_id: Option<&str>) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "group_id"),
            &[&group_id as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_sync_delay(&self, id: i64, ms: i32) -> Result<(), String> {
        self.db.execute(
            &sql::update_field(&self.db.dialect(), "sync_delay_ms"),
            &[&ms as &dyn rusqlite::types::ToSql, &id],
        )?;
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
        self.db.execute(
            &sql::save_playback_position(&self.db.dialect()),
            &[
                &position_ms as &dyn rusqlite::types::ToSql,
                &track_id,
                &source,
                &source_id,
                &id,
            ],
        )?;
        Ok(())
    }

    pub fn clear_playback_position(&self, id: i64) -> Result<(), String> {
        self.db.execute(
            &sql::clear_playback_position(&self.db.dialect()),
            &[&id as &dyn rusqlite::types::ToSql],
        )?;
        Ok(())
    }

    pub fn update_dsp(&self, id: i64, preset_id: Option<i64>, enabled: bool) -> Result<(), String> {
        let en = if enabled { 1i64 } else { 0i64 };
        self.db.execute(
            &sql::update_dsp(&self.db.dialect()),
            &[&preset_id as &dyn rusqlite::types::ToSql, &en, &id],
        )?;
        Ok(())
    }

    pub fn get_dsp_config(&self, id: i64) -> Result<(Option<i64>, bool), String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row(
            &sql::get_dsp_config(&self.db.dialect()),
            params![id],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
        )
        .map_err(|e| e.to_string())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.read_connection().lock().unwrap();
        conn.query_row(sql::count(), [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }
}

fn row_to_zone(row: &rusqlite::Row) -> Zone {
    Zone {
        id: row.get(0).ok(),
        name: row.get(1).unwrap_or_default(),
        output_type: row.get(2).ok(),
        output_device_id: row.get(3).ok(),
        volume: row.get(4).unwrap_or(50),
        muted: row.get::<_, i64>(5).unwrap_or(0) != 0,
        online: row.get::<_, i64>(6).unwrap_or(1) != 0,
        gapless_enabled: row.get::<_, i64>(7).unwrap_or(1) != 0,
        group_id: row.get(8).ok(),
        sync_delay_ms: row.get(9).unwrap_or(0),
        last_position_ms: row.get(10).unwrap_or(0),
        last_track_id: row.get(11).ok().flatten(),
        last_track_source: row.get(12).ok().flatten(),
        last_track_source_id: row.get(13).ok().flatten(),
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
        use crate::db::engine::{PostgresDialect, SqliteDialect};
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create(&s).contains("VALUES (?, ?, ?)"));
        assert!(sql::create(&p).contains("VALUES ($1, $2, $3)"));
        assert!(sql::update_field(&s, "volume").ends_with("SET volume = ? WHERE id = ?"));
        assert!(sql::update_field(&p, "volume").ends_with("SET volume = $1 WHERE id = $2"));
    }

    #[test]
    fn create_zone_during_open_transaction() {
        // Verify that zone creation is visible to list() even while a
        // scan transaction is still open on the write connection.
        //
        // This is the regression fix for forum bugs #2 (Dimitri,
        // macOS) and #6 (Dominique COMET, Windows): "zone créée"
        // flashed green then disappeared because list() used the
        // read-only connection whose snapshot pre-dated the commit.
        //
        // The fix: when the read connection returns 0 zones, fall
        // back to the write connection, which always sees its own
        // pending writes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = SqliteDb::open(path.to_str().unwrap()).unwrap();
        db.init_schema().unwrap();

        // Simulate scan starting a transaction
        db.execute_batch("BEGIN IMMEDIATE").unwrap();

        // Zone creation during the scan — inserts within the scan's transaction
        let repo = ZoneRepo::new(db.clone());
        let id = repo
            .create("Living Room", Some("dlna"), Some("uuid:123"))
            .unwrap();
        assert!(id > 0);

        // With the fix in place, list() now sees the zone even though
        // the scan hasn't committed yet — the write-connection
        // fallback exposes the pending insert.
        let zones_before_commit = repo.list().unwrap();
        assert_eq!(zones_before_commit.len(), 1);
        assert_eq!(zones_before_commit[0].name, "Living Room");

        // Commit the scan transaction
        db.execute_batch("COMMIT").unwrap();

        // Still visible after commit (now via the read connection).
        let zones_after_commit = repo.list().unwrap();
        assert_eq!(zones_after_commit.len(), 1);
        assert_eq!(zones_after_commit[0].name, "Living Room");
    }
}
