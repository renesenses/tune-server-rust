use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Local,
    Dlna,
    Airplay,
    Chromecast,
    Bluos,
    Squeezebox,
    Openhome,
    Bridge,
}

impl OutputType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Dlna => "dlna",
            Self::Airplay => "airplay",
            Self::Chromecast => "chromecast",
            Self::Bluos => "bluos",
            Self::Squeezebox => "squeezebox",
            Self::Openhome => "openhome",
            Self::Bridge => "bridge",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "local" => Some(Self::Local),
            "dlna" => Some(Self::Dlna),
            "airplay" => Some(Self::Airplay),
            "chromecast" => Some(Self::Chromecast),
            "bluos" => Some(Self::Bluos),
            "squeezebox" => Some(Self::Squeezebox),
            "openhome" => Some(Self::Openhome),
            "bridge" => Some(Self::Bridge),
            _ => None,
        }
    }

    pub fn is_network(&self) -> bool {
        !matches!(self, Self::Local)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneConfig {
    pub zone_id: i64,
    pub name: String,
    pub output_type: OutputType,
    pub output_device_id: Option<String>,
    pub volume: f64,
    pub muted: bool,
    pub enabled: bool,
    pub group_id: Option<String>,
    pub sync_delay_ms: i32,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        Self {
            zone_id: 0,
            name: "Default".into(),
            output_type: OutputType::Local,
            output_device_id: None,
            volume: 0.5,
            muted: false,
            enabled: true,
            group_id: None,
            sync_delay_ms: 0,
        }
    }
}

pub struct ZoneManager {
    db: SqliteDb,
    zones: HashMap<i64, ZoneConfig>,
    resume_on_recovery: HashMap<i64, bool>,
}

impl ZoneManager {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            db,
            zones: HashMap::new(),
            resume_on_recovery: HashMap::new(),
        }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS zones (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                output_type TEXT,
                output_device_id TEXT,
                volume INTEGER DEFAULT 50,
                muted INTEGER DEFAULT 0,
                online INTEGER DEFAULT 1,
                gapless_enabled INTEGER DEFAULT 1,
                group_id TEXT,
                sync_delay_ms INTEGER NOT NULL DEFAULT 0
            );",
        )
    }

    pub fn initialize(&mut self) -> Result<(), String> {
        let configs = {
            let conn = self.db.connection();
            let conn = conn.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, output_type, output_device_id, volume, \
                     muted, online, gapless_enabled, group_id, sync_delay_ms FROM zones WHERE online = 1",
                )
                .map_err(|e| e.to_string())?;

            stmt.query_map([], |row| {
                let ot_str: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
                Ok(ZoneConfig {
                    zone_id: row.get(0)?,
                    name: row.get(1)?,
                    output_type: OutputType::from_str(&ot_str).unwrap_or(OutputType::Local),
                    output_device_id: row.get(3)?,
                    volume: {
                        let v: i64 = row.get(4)?;
                        v as f64 / 100.0
                    },
                    muted: row.get::<_, i64>(5)? != 0,
                    enabled: row.get::<_, i64>(6)? != 0,
                    group_id: row.get(8)?,
                    sync_delay_ms: row.get(9)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };

        for cfg in configs {
            info!(zone_id = cfg.zone_id, name = %cfg.name, "zone_loaded");
            self.zones.insert(cfg.zone_id, cfg);
        }

        if self.zones.is_empty() {
            let default = self.create_zone("Default", OutputType::Local, None)?;
            info!(zone_id = default.zone_id, "default_zone_created");
        }

        Ok(())
    }

    pub fn create_zone(
        &mut self,
        name: &str,
        output_type: OutputType,
        output_device_id: Option<&str>,
    ) -> Result<ZoneConfig, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "INSERT INTO zones (name, output_type, output_device_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![name, output_type.as_str(), output_device_id],
        )
        .map_err(|e| e.to_string())?;

        let id = conn.last_insert_rowid();
        let cfg = ZoneConfig {
            zone_id: id,
            name: name.into(),
            output_type,
            output_device_id: output_device_id.map(String::from),
            ..Default::default()
        };
        self.zones.insert(id, cfg.clone());
        Ok(cfg)
    }

    pub fn delete_zone(&mut self, zone_id: i64) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute("DELETE FROM zones WHERE id = ?1", [zone_id])
            .map_err(|e| e.to_string())?;
        self.zones.remove(&zone_id);
        Ok(())
    }

    pub fn rename_zone(&mut self, zone_id: i64, name: &str) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE zones SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, zone_id],
        )
        .map_err(|e| e.to_string())?;
        if let Some(z) = self.zones.get_mut(&zone_id) {
            z.name = name.into();
        }
        Ok(())
    }

    pub fn set_volume(&mut self, zone_id: i64, volume: f64) -> Result<(), String> {
        let volume = volume.clamp(0.0, 1.0);
        let db_volume = (volume * 100.0).round() as i64;
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE zones SET volume = ?1 WHERE id = ?2",
            rusqlite::params![db_volume, zone_id],
        )
        .map_err(|e| e.to_string())?;
        if let Some(z) = self.zones.get_mut(&zone_id) {
            z.volume = volume;
        }
        Ok(())
    }

    pub fn set_output(
        &mut self,
        zone_id: i64,
        output_type: OutputType,
        device_id: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute(
            "UPDATE zones SET output_type = ?1, output_device_id = ?2 WHERE id = ?3",
            rusqlite::params![output_type.as_str(), device_id, zone_id],
        )
        .map_err(|e| e.to_string())?;
        if let Some(z) = self.zones.get_mut(&zone_id) {
            z.output_type = output_type;
            z.output_device_id = device_id.map(String::from);
        }
        Ok(())
    }

    pub fn get_zone(&self, zone_id: i64) -> Option<&ZoneConfig> {
        self.zones.get(&zone_id)
    }

    pub fn list_zones(&self) -> Vec<&ZoneConfig> {
        self.zones.values().collect()
    }

    pub fn zone_ids(&self) -> Vec<i64> {
        self.zones.keys().copied().collect()
    }

    pub fn mark_resume_on_recovery(&mut self, zone_id: i64, was_playing: bool) {
        self.resume_on_recovery.insert(zone_id, was_playing);
    }

    pub fn should_resume(&self, zone_id: i64) -> bool {
        self.resume_on_recovery
            .get(&zone_id)
            .copied()
            .unwrap_or(false)
    }

    pub fn clear_resume(&mut self, zone_id: i64) {
        self.resume_on_recovery.remove(&zone_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> ZoneManager {
        let db = SqliteDb::open_in_memory().unwrap();
        let mgr = ZoneManager::new(db);
        mgr.setup_table().unwrap();
        mgr
    }

    #[test]
    fn output_type_roundtrip() {
        for ot in [
            OutputType::Local,
            OutputType::Dlna,
            OutputType::Airplay,
            OutputType::Chromecast,
            OutputType::Bluos,
            OutputType::Openhome,
            OutputType::Bridge,
        ] {
            assert_eq!(OutputType::from_str(ot.as_str()), Some(ot));
        }
    }

    #[test]
    fn network_detection() {
        assert!(!OutputType::Local.is_network());
        assert!(OutputType::Dlna.is_network());
        assert!(OutputType::Chromecast.is_network());
    }

    #[test]
    fn create_and_list_zones() {
        let mut mgr = setup();
        mgr.create_zone("Living Room", OutputType::Dlna, Some("uuid:1234"))
            .unwrap();
        mgr.create_zone("Kitchen", OutputType::Chromecast, None)
            .unwrap();

        let zones = mgr.list_zones();
        assert_eq!(zones.len(), 2);
    }

    #[test]
    fn initialize_creates_default() {
        let mut mgr = setup();
        mgr.initialize().unwrap();
        assert!(!mgr.list_zones().is_empty());
        assert!(mgr.list_zones().iter().any(|z| z.name == "Default"));
    }

    #[test]
    fn rename_zone() {
        let mut mgr = setup();
        let z = mgr.create_zone("Old", OutputType::Local, None).unwrap();
        mgr.rename_zone(z.zone_id, "New").unwrap();
        assert_eq!(mgr.get_zone(z.zone_id).unwrap().name, "New");
    }

    #[test]
    fn delete_zone() {
        let mut mgr = setup();
        let z = mgr.create_zone("Temp", OutputType::Local, None).unwrap();
        mgr.delete_zone(z.zone_id).unwrap();
        assert!(mgr.get_zone(z.zone_id).is_none());
    }

    #[test]
    fn set_volume_clamped() {
        let mut mgr = setup();
        let z = mgr.create_zone("Test", OutputType::Local, None).unwrap();
        mgr.set_volume(z.zone_id, 1.5).unwrap();
        assert_eq!(mgr.get_zone(z.zone_id).unwrap().volume, 1.0);
        mgr.set_volume(z.zone_id, -0.5).unwrap();
        assert_eq!(mgr.get_zone(z.zone_id).unwrap().volume, 0.0);
    }

    #[test]
    fn resume_tracking() {
        let mut mgr = setup();
        mgr.mark_resume_on_recovery(1, true);
        assert!(mgr.should_resume(1));
        assert!(!mgr.should_resume(2));
        mgr.clear_resume(1);
        assert!(!mgr.should_resume(1));
    }
}
