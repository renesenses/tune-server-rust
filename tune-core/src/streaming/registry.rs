use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};

use super::traits::StreamingService;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// `RwLock` et non `Mutex` : les lectures peuvent se faire en parallele.
///
/// Le `Mutex` donnait une exclusivite que les lectures ne peuvent PAS utiliser
/// — toutes les methodes de lecture du trait sont en `&self`, elles n'ont aucun
/// moyen de muter quoi que ce soit, meme seules. Elles se serialisaient donc
/// mutuellement sans contrepartie : la page decouverte tire sept requetes
/// editoriales, qui faisaient la queue une par une derriere ce verrou (#1969,
/// lot 6 de #1621).
///
/// Les huit methodes en `&mut self` — `refresh_if_needed`, `logout`,
/// `set_enabled`, les favoris… — prennent `.write()`. Le compilateur le fait
/// respecter : une methode `&mut self` NE COMPILE PAS sous un garde de lecture.
/// Il n'y a donc pas de cas silencieusement faux.
pub struct ServiceRegistry {
    services: HashMap<String, Arc<RwLock<Box<dyn StreamingService>>>>,
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
        self.services.insert(name, Arc::new(RwLock::new(service)));
    }

    pub fn get(&self, name: &str) -> Option<Arc<RwLock<Box<dyn StreamingService>>>> {
        self.services.get(name).cloned()
    }

    pub fn list(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    pub async fn status_all(&self) -> Vec<serde_json::Value> {
        let mut results = Vec::new();
        for (name, svc) in &self.services {
            let svc = svc.read().await;
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
            let svc = svc.read().await;
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
        let svc = svc.read().await;
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
        let svc = svc.read().await;
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
        let svc = svc.read().await;
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
                let mut svc_locked = svc.write().await;
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
                let mut svc = svc.write().await;
                if svc.restore_tokens(&tokens) {
                    info!(service = %name, "tokens_restored");
                    // A row in a superseded shape (today: the Qobuz plaintext
                    // password) is rewritten here rather than waiting for the
                    // next refresh — a token that never expires would leave the
                    // secret on disk indefinitely.
                    if svc.tokens_need_rewrite()
                        && let Some(clean) = svc.save_tokens()
                    {
                        settings.set(&key, &clean.to_string()).ok();
                        warn!(service = %name, "tokens_rewritten_dropping_stale_fields");
                    }
                    svc.post_restore().await;
                    // `post_restore` probes the token. If the provider refused
                    // it, the row cannot be used again — drop it rather than
                    // reload it on every boot.
                    if svc.session_expired() {
                        settings.delete(&key).ok();
                        warn!(service = %name, "expired_session_row_deleted");
                    }
                }
            }
        }
    }
}
