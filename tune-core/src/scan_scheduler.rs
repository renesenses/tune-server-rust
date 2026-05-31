use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::info;

use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScanSchedule {
    pub enabled: bool,
    pub interval_hours: u64,
    pub scan_at_startup: bool,
    pub directories: Vec<String>,
}

impl Default for ScanSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_hours: 24,
            scan_at_startup: true,
            directories: Vec::new(),
        }
    }
}

pub struct ScanScheduler {
    db: SqliteDb,
    schedule: Mutex<ScanSchedule>,
    running: Mutex<bool>,
}

impl ScanScheduler {
    pub fn new(db: SqliteDb) -> Self {
        let schedule = load_schedule(&db);
        Self {
            db,
            schedule: Mutex::new(schedule),
            running: Mutex::new(false),
        }
    }

    pub async fn get_schedule(&self) -> ScanSchedule {
        self.schedule.lock().await.clone()
    }

    pub async fn update_schedule(&self, schedule: ScanSchedule) {
        save_schedule(&self.db, &schedule);
        *self.schedule.lock().await = schedule;
        info!("scan_schedule_updated");
    }

    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }

    pub fn spawn(self: Arc<Self>, scan_fn: Arc<dyn Fn(Vec<String>) + Send + Sync>) {
        tokio::spawn(async move {
            let schedule = self.schedule.lock().await.clone();
            if schedule.scan_at_startup && schedule.enabled && !schedule.directories.is_empty() {
                info!("startup_scan_triggered");
                *self.running.lock().await = true;
                scan_fn(schedule.directories.clone());
                *self.running.lock().await = false;
            }

            loop {
                let schedule = self.schedule.lock().await.clone();
                if !schedule.enabled || schedule.interval_hours == 0 {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }

                let sleep_duration = Duration::from_secs(schedule.interval_hours * 3600);
                tokio::time::sleep(sleep_duration).await;

                let schedule = self.schedule.lock().await.clone();
                if !schedule.enabled || schedule.directories.is_empty() {
                    continue;
                }

                if *self.running.lock().await {
                    info!("scan_skipped_already_running");
                    continue;
                }

                info!(
                    dirs = ?schedule.directories,
                    "scheduled_scan_triggered"
                );
                *self.running.lock().await = true;
                scan_fn(schedule.directories.clone());
                *self.running.lock().await = false;
            }
        });
    }

    pub async fn status(&self) -> serde_json::Value {
        let schedule = self.schedule.lock().await.clone();
        let running = *self.running.lock().await;
        serde_json::json!({
            "enabled": schedule.enabled,
            "interval_hours": schedule.interval_hours,
            "scan_at_startup": schedule.scan_at_startup,
            "directories": schedule.directories,
            "running": running,
        })
    }
}

fn load_schedule(db: &SqliteDb) -> ScanSchedule {
    let settings = SettingsRepo::new(db.clone());
    let json_str = settings
        .get("scan_schedule")
        .ok()
        .flatten()
        .unwrap_or_default();

    if json_str.is_empty() {
        return ScanSchedule::default();
    }

    serde_json::from_str(&json_str).unwrap_or_default()
}

fn save_schedule(db: &SqliteDb, schedule: &ScanSchedule) {
    let settings = SettingsRepo::new(db.clone());
    if let Ok(json) = serde_json::to_string(schedule) {
        settings.set("scan_schedule", &json).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_schedule() {
        let s = ScanSchedule::default();
        assert!(!s.enabled);
        assert_eq!(s.interval_hours, 24);
        assert!(s.scan_at_startup);
        assert!(s.directories.is_empty());
    }

    #[test]
    fn schedule_serialize() {
        let s = ScanSchedule {
            enabled: true,
            interval_hours: 12,
            scan_at_startup: false,
            directories: vec!["/music".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: ScanSchedule = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.interval_hours, 12);
        assert_eq!(parsed.directories.len(), 1);
    }

    #[tokio::test]
    async fn scheduler_status() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let scheduler = ScanScheduler::new(db);

        let status = scheduler.status().await;
        assert_eq!(status["enabled"], false);
        assert_eq!(status["running"], false);
    }

    #[tokio::test]
    async fn update_schedule() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let scheduler = ScanScheduler::new(db);

        let new_schedule = ScanSchedule {
            enabled: true,
            interval_hours: 6,
            scan_at_startup: true,
            directories: vec!["/music/flac".into()],
        };
        scheduler.update_schedule(new_schedule).await;

        let current = scheduler.get_schedule().await;
        assert!(current.enabled);
        assert_eq!(current.interval_hours, 6);
    }
}
