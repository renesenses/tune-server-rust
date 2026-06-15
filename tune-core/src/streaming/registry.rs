use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;

use super::traits::StreamingService;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

pub struct ServiceRegistry {
    services: HashMap<String, Arc<Mutex<Box<dyn StreamingService>>>>,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }

    pub fn register(&mut self, service: Box<dyn StreamingService>) {
        let name = service.name().to_string();
        self.services.insert(name, Arc::new(Mutex::new(service)));
    }

    pub fn get(&self, name: &str) -> Option<Arc<Mutex<Box<dyn StreamingService>>>> {
        self.services.get(name).cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    pub async fn status_all(&self) -> Vec<serde_json::Value> {
        let mut results = Vec::new();
        for (name, svc) in &self.services {
            let svc = svc.lock().await;
            let status = svc.auth_status().await;
            results.push(serde_json::json!({
                "name": name,
                "enabled": svc.enabled(),
                "authenticated": status.authenticated,
                "username": status.username,
                "subscription": status.subscription,
            }));
        }
        results
    }

    pub async fn save_all_tokens(&self, db: &Arc<dyn DbBackend>) {
        let settings = SettingsRepo::with_backend(db.clone());
        for (name, svc) in &self.services {
            let svc = svc.lock().await;
            if let Some(tokens) = svc.save_tokens() {
                let key = format!("auth_tokens_{name}");
                settings.set(&key, &tokens.to_string()).ok();
                info!(service = %name, "tokens_saved");
            }
        }
    }

    pub async fn get_stream_url(
        &self,
        service_name: &str,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<String, String> {
        let svc = self
            .services
            .get(service_name)
            .ok_or_else(|| format!("service not found: {service_name}"))?;
        let svc = svc.lock().await;
        let stream_url = svc.get_track_url(track_id, quality).await?;
        Ok(stream_url.url)
    }

    pub async fn get_album_tracks(
        &self,
        service_name: &str,
        album_id: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let svc = self
            .services
            .get(service_name)
            .ok_or_else(|| format!("service not found: {service_name}"))?;
        let svc = svc.lock().await;
        let tracks = svc.get_album_tracks(album_id).await?;
        Ok(tracks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "artist": t.artist,
                    "album": t.album,
                })
            })
            .collect())
    }

    pub async fn get_playlist_tracks(
        &self,
        service_name: &str,
        playlist_id: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let svc = self
            .services
            .get(service_name)
            .ok_or_else(|| format!("service not found: {service_name}"))?;
        let svc = svc.lock().await;
        let tracks = svc.get_playlist_tracks(playlist_id).await?;
        Ok(tracks
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "artist": t.artist,
                    "album": t.album,
                })
            })
            .collect())
    }

    pub async fn restore_all_tokens(&self, db: &Arc<dyn DbBackend>) {
        let settings = SettingsRepo::with_backend(db.clone());
        for (name, svc) in &self.services {
            // Restore enabled/disabled state
            let enabled_key = format!("streaming_{name}_enabled");
            if let Some(val) = settings.get(&enabled_key).ok().flatten() {
                let mut svc_locked = svc.lock().await;
                match val.as_str() {
                    "true" => svc_locked.set_enabled(true),
                    "false" => svc_locked.set_enabled(false),
                    _ => {}
                }
                drop(svc_locked);
            }

            // Restore auth tokens
            let key = format!("auth_tokens_{name}");
            if let Some(json_str) = settings.get(&key).ok().flatten()
                && let Ok(tokens) = serde_json::from_str(&json_str)
            {
                let mut svc = svc.lock().await;
                if svc.restore_tokens(&tokens) {
                    info!(service = %name, "tokens_restored");
                    svc.post_restore().await;
                }
            }
        }
    }
}
