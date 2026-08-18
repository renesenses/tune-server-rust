use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

pub struct SettingsRepo {
    db: Arc<dyn DbBackend>,
}

/// Engine-agnostic SQL builders. They live as free functions so the
/// future PostgresRepo can call them with `PostgresDialect` while the
/// SQLite repo below uses `SqliteDialect`.
pub mod sql {
    use super::SqlDialect;

    pub fn get_by_key<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT value FROM settings WHERE key = {}",
            d.placeholder(1)
        )
    }

    pub fn delete_by_key<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM settings WHERE key = {}", d.placeholder(1))
    }

    pub fn list_all() -> &'static str {
        "SELECT key, value FROM settings ORDER BY key"
    }

    /// Upsert via the SQL standard `ON CONFLICT` form (SQLite 3.24+,
    /// PostgreSQL 9.5+). Both dialects use the same placeholders.
    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO settings (key, value, updated_at) \
             VALUES ({}, {}, {}) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }
}

impl SettingsRepo {
    /// Backward-compatible constructor for the existing call sites.
    /// Wraps the concrete `SqliteDb` in an `Arc<dyn DbBackend>` so the
    /// internal storage matches the new trait-object form. Same observable
    /// behavior as before phase 5 of the PG roadmap.
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    /// New constructor used by callers that already hold an
    /// `Arc<dyn DbBackend>` (Postgres or SQLite).
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

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let sql = self.dialect_sql(sql::get_by_key, sql::get_by_key);
        let params: [&dyn ToSqlValue; 1] = [&key];
        // Use query_one_strong to read through the write connection.
        // Settings are frequently read immediately after a write (e.g.
        // saving a Discogs token then checking discogs_token_set in
        // get_config). The read-only WAL snapshot may lag behind the
        // writer, returning stale NULL for a key that was just upserted.
        match self.db.query_one_strong(&sql, &params)? {
            None => Ok(None),
            Some(row) => Ok(row.first().and_then(|v| v.as_string())),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 3] = [&key, &value, &now];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_by_key, sql::delete_by_key);
        let params: [&dyn ToSqlValue; 1] = [&key];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn all(&self) -> Result<Vec<(String, String)>, String> {
        // Use query_many_strong for the same WAL snapshot reason as get().
        let rows = self.db.query_many_strong(sql::list_all(), &[])?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                let k = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
                let v = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
                (k, v)
            })
            .collect())
    }

    // --- AirPlay 2 pairing credentials (keyed per device_id) --------------
    //
    // Long-term secrets from a successful HomeKit-style pairing: our controller
    // Ed25519 seed + the accessory's long-term public key (`AccessoryLTPK`) +
    // its pairing identifier. Stored as JSON under `airplay2_pairing:<id>` in
    // the same key/value settings table (no schema change needed). The values
    // are populated by the pair-setup handshake in a later increment; the
    // storage/accessor plumbing lives here now.

    /// Persist pairing credentials for a device.
    pub fn set_airplay_pairing(
        &self,
        device_id: &str,
        creds: &AirplayPairingRecord,
    ) -> Result<(), String> {
        let json =
            serde_json::to_string(creds).map_err(|e| format!("serialize airplay pairing: {e}"))?;
        self.set(&airplay_pairing_key(device_id), &json)
    }

    /// Load pairing credentials for a device, if we have paired with it.
    pub fn get_airplay_pairing(
        &self,
        device_id: &str,
    ) -> Result<Option<AirplayPairingRecord>, String> {
        match self.get(&airplay_pairing_key(device_id))? {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| format!("deserialize airplay pairing: {e}")),
        }
    }

    /// Forget a device's pairing (e.g. user re-pairs or removes it).
    pub fn delete_airplay_pairing(&self, device_id: &str) -> Result<(), String> {
        self.delete(&airplay_pairing_key(device_id))
    }
}

/// Settings key namespace for AirPlay 2 pairing records.
fn airplay_pairing_key(device_id: &str) -> String {
    format!("airplay2_pairing:{device_id}")
}

/// Stored AirPlay 2 / HomeKit pairing credentials for one accessory.
///
/// Byte arrays are hex-encoded strings so the record is human-readable JSON in
/// the settings table. Kept independent of `tune_core::outputs::airplay2` so the
/// DB layer has no dependency on the crypto module.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AirplayPairingRecord {
    /// Our controller Ed25519 seed (32 bytes, hex) — secret.
    pub our_ed25519_seed_hex: String,
    /// The accessory's long-term public key (`AccessoryLTPK`, 32 bytes, hex).
    pub accessory_ltpk_hex: String,
    /// The accessory pairing identifier (its `AccessoryPairingID` string).
    pub accessory_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_repo() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        SettingsRepo::new(db)
    }

    #[test]
    fn settings_crud() {
        let repo = fresh_repo();

        assert!(repo.get("music_dirs").unwrap().is_none());

        repo.set("music_dirs", r#"["/music"]"#).unwrap();
        assert_eq!(repo.get("music_dirs").unwrap().unwrap(), r#"["/music"]"#);

        repo.set("music_dirs", r#"["/music","/nas"]"#).unwrap();
        assert_eq!(
            repo.get("music_dirs").unwrap().unwrap(),
            r#"["/music","/nas"]"#
        );

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 1);

        repo.delete("music_dirs").unwrap();
        assert!(repo.get("music_dirs").unwrap().is_none());
    }

    #[test]
    fn settings_multiple_keys() {
        let repo = fresh_repo();
        repo.set("key1", "value1").unwrap();
        repo.set("key2", "value2").unwrap();
        repo.set("key3", "value3").unwrap();

        let all = repo.all().unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "key1");
        assert_eq!(all[1].0, "key2");
        assert_eq!(all[2].0, "key3");
    }

    #[test]
    fn settings_overwrite() {
        let repo = fresh_repo();
        repo.set("theme", "dark").unwrap();
        repo.set("theme", "light").unwrap();
        assert_eq!(repo.get("theme").unwrap().unwrap(), "light");
        assert_eq!(repo.all().unwrap().len(), 1);
    }

    #[test]
    fn settings_delete_nonexistent() {
        let repo = fresh_repo();
        repo.delete("nonexistent").unwrap();
    }

    #[test]
    fn settings_empty_value() {
        let repo = fresh_repo();
        repo.set("empty", "").unwrap();
        assert_eq!(repo.get("empty").unwrap().unwrap(), "");
    }

    #[test]
    fn settings_json_value() {
        let repo = fresh_repo();
        let json = r#"{"enabled":true,"services":["tidal","qobuz"]}"#;
        repo.set("streaming_config", json).unwrap();
        assert_eq!(repo.get("streaming_config").unwrap().unwrap(), json);
    }

    #[test]
    fn settings_unicode_key_and_value() {
        let repo = fresh_repo();
        repo.set("nom_utilisateur", "Rene").unwrap();
        assert_eq!(repo.get("nom_utilisateur").unwrap().unwrap(), "Rene");
    }

    #[test]
    fn with_backend_constructor() {
        // Verify the new `Arc<dyn DbBackend>` constructor works too.
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = SettingsRepo::with_backend(backend);
        repo.set("k", "v").unwrap();
        assert_eq!(repo.get("k").unwrap().unwrap(), "v");
    }

    #[test]
    fn sql_builders_emit_sqlite_placeholders() {
        let d = SqliteDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = ?"
        );
        assert_eq!(sql::delete_by_key(&d), "DELETE FROM settings WHERE key = ?");
        assert_eq!(
            sql::list_all(),
            "SELECT key, value FROM settings ORDER BY key"
        );
    }

    #[test]
    fn sql_builders_emit_postgres_placeholders() {
        let d = PostgresDialect;
        assert_eq!(
            sql::get_by_key(&d),
            "SELECT value FROM settings WHERE key = $1"
        );
        assert_eq!(
            sql::delete_by_key(&d),
            "DELETE FROM settings WHERE key = $1"
        );
    }

    #[test]
    fn airplay_pairing_roundtrip_per_device() {
        let repo = fresh_repo();
        let dev = "airplay2:AA-BB-CC";

        // Nothing stored yet.
        assert!(repo.get_airplay_pairing(dev).unwrap().is_none());

        let rec = AirplayPairingRecord {
            our_ed25519_seed_hex: "00".repeat(32),
            accessory_ltpk_hex: "ab".repeat(32),
            accessory_id: "AABBCCDDEEFF".into(),
        };
        repo.set_airplay_pairing(dev, &rec).unwrap();

        // Round-trips to the exact same record.
        assert_eq!(repo.get_airplay_pairing(dev).unwrap().unwrap(), rec);

        // A different device is isolated.
        assert!(
            repo.get_airplay_pairing("airplay2:other")
                .unwrap()
                .is_none()
        );

        // Deletion forgets it.
        repo.delete_airplay_pairing(dev).unwrap();
        assert!(repo.get_airplay_pairing(dev).unwrap().is_none());
    }

    #[test]
    fn airplay_pairing_key_is_namespaced() {
        assert_eq!(
            airplay_pairing_key("airplay2:x"),
            "airplay2_pairing:airplay2:x"
        );
    }
}
