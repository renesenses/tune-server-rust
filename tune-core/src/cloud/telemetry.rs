use serde::Serialize;
use tracing::{debug, info};

use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

#[derive(Debug, Serialize)]
struct TelemetryPayload {
    instance_id: String,
    version: String,
    os: String,
    arch: String,
    tracks: i64,
    albums: i64,
    zones: i64,
    uptime_hours: u64,
    engine: String,
}

pub struct TelemetryReporter;

impl TelemetryReporter {
    /// Get or create a persistent instance ID for this server.
    pub fn get_or_create_instance_id(settings: &SettingsRepo) -> String {
        match settings.get("instance_id").ok().flatten() {
            Some(id) if !id.is_empty() => id,
            _ => {
                let id = uuid::Uuid::new_v4().to_string();
                settings.set("instance_id", &id).ok();
                id
            }
        }
    }

    /// Check if telemetry is enabled.
    pub fn is_enabled(settings: &SettingsRepo) -> bool {
        settings.get("telemetry_enabled").ok().flatten().as_deref() == Some("true")
    }

    /// Send a telemetry report to mozaiklabs.fr.
    /// Only sends if `telemetry_enabled` is "true" in settings.
    pub async fn report(db: &SqliteDb, base_url: Option<&str>, uptime_hours: u64) {
        let settings = SettingsRepo::new(db.clone());

        if !Self::is_enabled(&settings) {
            return;
        }

        let instance_id = Self::get_or_create_instance_id(&settings);
        let base = base_url.unwrap_or(DEFAULT_BASE_URL).trim_end_matches('/');

        let tracks = crate::db::track_repo::TrackRepo::new(db.clone())
            .count()
            .unwrap_or(0);
        let albums = crate::db::album_repo::AlbumRepo::new(db.clone())
            .count()
            .unwrap_or(0);
        let zones = crate::db::zone_repo::ZoneRepo::new(db.clone())
            .list()
            .map(|z| z.len() as i64)
            .unwrap_or(0);

        let payload = TelemetryPayload {
            instance_id,
            version: crate::version().to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            tracks,
            albums,
            zones,
            uptime_hours,
            engine: "rust".to_string(),
        };

        let url = format!("{base}/api/v1/telemetry");
        let client = crate::http::client::shared();

        match client.post(&url).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("telemetry_reported");
            }
            Ok(resp) => {
                let status = resp.status();
                debug!(status = %status, "telemetry_report_rejected");
            }
            Err(e) => {
                debug!(error = %e, "telemetry_report_failed");
            }
        }
    }

    /// Spawn a background task that reports telemetry once after 30s, then every 24h.
    pub fn spawn(db: SqliteDb) {
        tokio::spawn(async move {
            // Initial delay
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            let start = std::time::Instant::now();
            loop {
                let uptime_hours = start.elapsed().as_secs() / 3600;
                Self::report(&db, None, uptime_hours).await;
                // Sleep 24h
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn fresh_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn instance_id_persists() {
        let db = fresh_db();
        let settings = SettingsRepo::new(db);
        let id1 = TelemetryReporter::get_or_create_instance_id(&settings);
        let id2 = TelemetryReporter::get_or_create_instance_id(&settings);
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
    }

    #[test]
    fn telemetry_disabled_by_default() {
        let db = fresh_db();
        let settings = SettingsRepo::new(db);
        assert!(!TelemetryReporter::is_enabled(&settings));
    }

    #[test]
    fn telemetry_opt_in() {
        let db = fresh_db();
        let settings = SettingsRepo::new(db);
        settings.set("telemetry_enabled", "true").unwrap();
        assert!(TelemetryReporter::is_enabled(&settings));
    }
}
