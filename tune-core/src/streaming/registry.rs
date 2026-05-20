use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::info;

use super::traits::StreamingService;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;

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

    pub async fn save_all_tokens(&self, db: &SqliteDb) {
        let settings = SettingsRepo::new(db.clone());
        for (name, svc) in &self.services {
            let svc = svc.lock().await;
            if let Some(tokens) = svc.save_tokens() {
                let key = format!("auth_tokens_{name}");
                settings.set(&key, &tokens.to_string()).ok();
                info!(service = %name, "tokens_saved");
            }
        }
    }

    pub async fn restore_all_tokens(&self, db: &SqliteDb) {
        let settings = SettingsRepo::new(db.clone());
        for (name, svc) in &self.services {
            let key = format!("auth_tokens_{name}");
            if let Some(json_str) = settings.get(&key).ok().flatten()
                && let Ok(tokens) = serde_json::from_str(&json_str) {
                    let mut svc = svc.lock().await;
                    if svc.restore_tokens(&tokens) {
                        info!(service = %name, "tokens_restored");
                    }
                }
        }
    }
}
