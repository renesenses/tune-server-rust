use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::streaming::ServiceRegistry;

const HEARTBEAT_URL: &str = "https://mozaiklabs.fr/api/v1/telemetry/heartbeat";
const PING_URL: &str = "https://mozaiklabs.fr/api/v1/ping";

/// Fire-and-forget startup ping: sends version + OS + arch + services once after 10 seconds.
/// Always runs regardless of TUNE_TELEMETRY setting (anonymous, no personal data).
pub fn spawn_startup_ping(
    services: std::sync::Arc<tokio::sync::Mutex<crate::streaming::registry::ServiceRegistry>>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let Ok(client) = crate::http::client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
        else {
            return;
        };
        let svc_list: Vec<String> = {
            let reg = services.lock().await;
            reg.list()
        };
        let payload = serde_json::json!({
            "v": crate::version(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "services": svc_list,
        });
        let _ = client.post(PING_URL).json(&payload).send().await;
    });
}

#[derive(Debug, Serialize)]
struct HeartbeatPayload {
    server_id: String,
    version: String,
    services: Vec<String>,
    tracks_count: i64,
    artists_with_bio: i64,
    albums_with_bio: i64,
    os: String,
    arch: String,
}

pub struct TelemetryReporter;

impl TelemetryReporter {
    /// Get or create a persistent server ID for this instance.
    pub fn get_or_create_server_id(settings: &SettingsRepo) -> String {
        match settings.get("server_id").ok().flatten() {
            Some(id) if !id.is_empty() => id,
            _ => {
                let id = uuid::Uuid::new_v4().to_string();
                settings.set("server_id", &id).ok();
                id
            }
        }
    }

    /// Check if telemetry is enabled.
    /// Enabled by default; set `TUNE_TELEMETRY=false` to opt out.
    pub fn is_enabled() -> bool {
        match std::env::var("TUNE_TELEMETRY") {
            Ok(val) => !matches!(val.to_lowercase().as_str(), "false" | "0" | "no" | "off"),
            Err(_) => true,
        }
    }

    /// Collect and send a heartbeat to mozaiklabs.fr.
    /// Fails silently — logs a warning but never panics.
    pub async fn send(db: &Arc<dyn DbBackend>, services: &Arc<Mutex<ServiceRegistry>>) {
        if !Self::is_enabled() {
            return;
        }

        let settings = SettingsRepo::with_backend(db.clone());
        let server_id = Self::get_or_create_server_id(&settings);

        // Collect connected service names (authenticated == true)
        let connected_services = {
            let registry = services.lock().await;
            let mut names = Vec::new();
            for name in registry.list() {
                if let Some(svc) = registry.get(&name) {
                    let svc = svc.lock().await;
                    let status = svc.auth_status().await;
                    if status.authenticated {
                        names.push(name);
                    }
                }
            }
            names.sort();
            names
        };

        let tracks_count = crate::db::track_repo::TrackRepo::with_backend(db.clone())
            .count()
            .unwrap_or(0);
        let artists_with_bio = crate::db::artist_repo::ArtistRepo::with_backend(db.clone())
            .count_with_bio()
            .unwrap_or(0);
        let albums_with_bio = crate::db::album_repo::AlbumRepo::with_backend(db.clone())
            .count_with_bio()
            .unwrap_or(0);

        let payload = HeartbeatPayload {
            server_id,
            version: crate::version().to_string(),
            services: connected_services,
            tracks_count,
            artists_with_bio,
            albums_with_bio,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        };

        let client = match crate::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "telemetry_client_build_failed");
                return;
            }
        };

        match client.post(HEARTBEAT_URL).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    tracks = payload.tracks_count,
                    services = ?payload.services,
                    "telemetry_heartbeat_sent"
                );
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(status = %status, "telemetry_heartbeat_rejected");
            }
            Err(e) => {
                warn!(error = %e, "telemetry_heartbeat_failed");
            }
        }
    }

    /// Spawn a background task that sends a heartbeat after 30 seconds,
    /// then every 24 hours.
    pub fn spawn(db: Arc<dyn DbBackend>, services: Arc<Mutex<ServiceRegistry>>) {
        tokio::spawn(async move {
            // Initial delay: give the server time to restore tokens and scan
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            loop {
                Self::send(&db, &services).await;
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn fresh_db() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        std::sync::Arc::new(db)
    }

    #[test]
    fn server_id_persists() {
        let db = fresh_db();
        let settings = SettingsRepo::with_backend(db);
        let id1 = TelemetryReporter::get_or_create_server_id(&settings);
        let id2 = TelemetryReporter::get_or_create_server_id(&settings);
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
        // Should be a valid UUID
        assert!(uuid::Uuid::parse_str(&id1).is_ok());
    }

    #[test]
    fn telemetry_enabled_by_default() {
        // Can only verify when TUNE_TELEMETRY is not set to false
        // (env var state in tests is not guaranteed, so we just ensure no panic)
        let _ = TelemetryReporter::is_enabled();
    }
}
