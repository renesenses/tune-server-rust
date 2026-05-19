use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::StreamingService;

pub struct ServiceRegistry {
    services: HashMap<String, Arc<Mutex<Box<dyn StreamingService>>>>,
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
}
