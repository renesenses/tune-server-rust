use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: Option<i64>,
    pub name: String,
    pub output_type: Option<String>,
    pub output_device_id: Option<String>,
    pub volume: i32,
    pub muted: bool,
    pub online: bool,
}

pub struct ZoneRepo {
    db: SqliteDb,
}

impl ZoneRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<Zone>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, output_type, output_device_id, volume, muted, online FROM zones WHERE id = ?")
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_zone(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<Zone>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, output_type, output_device_id, volume, muted, online FROM zones ORDER BY name")
            .map_err(|e| e.to_string())?;
        let zones = stmt
            .query_map([], |row| Ok(row_to_zone(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();
        Ok(zones)
    }

    pub fn create(&self, name: &str, output_type: Option<&str>, output_device_id: Option<&str>) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO zones (name, output_type, output_device_id) VALUES (?, ?, ?)",
            &[&name as &dyn rusqlite::types::ToSql, &output_type, &output_device_id],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn update_volume(&self, id: i64, volume: i32) -> Result<(), String> {
        self.db.execute(
            "UPDATE zones SET volume = ? WHERE id = ?",
            &[&volume as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_muted(&self, id: i64, muted: bool) -> Result<(), String> {
        let val = if muted { 1i64 } else { 0i64 };
        self.db.execute(
            "UPDATE zones SET muted = ? WHERE id = ?",
            &[&val as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_name(&self, id: i64, name: &str) -> Result<(), String> {
        self.db.execute(
            "UPDATE zones SET name = ? WHERE id = ?",
            &[&name as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_output_device(&self, id: i64, device_id: &str) -> Result<(), String> {
        self.db.execute(
            "UPDATE zones SET output_device_id = ? WHERE id = ?",
            &[&device_id as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_output_type(&self, id: i64, output_type: &str) -> Result<(), String> {
        self.db.execute(
            "UPDATE zones SET output_type = ? WHERE id = ?",
            &[&output_type as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn update_online(&self, id: i64, online: bool) -> Result<(), String> {
        let val = if online { 1i64 } else { 0i64 };
        self.db.execute(
            "UPDATE zones SET online = ? WHERE id = ?",
            &[&val as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn set_online_by_device(&self, device_id: &str, online: bool) -> Result<usize, String> {
        let val = if online { 1i64 } else { 0i64 };
        let conn = self.db.connection().lock().unwrap();
        conn.execute(
            "UPDATE zones SET online = ? WHERE output_device_id = ?",
            rusqlite::params![val, device_id],
        ).map(|n| n).map_err(|e| e.to_string())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db.execute("DELETE FROM zones WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM zones", [], |row| row.get(0))
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

        let id = repo.create("Living Room", Some("dlna"), Some("uuid:123")).unwrap();
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
}
