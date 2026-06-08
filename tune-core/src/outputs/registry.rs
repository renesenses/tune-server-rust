use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::OutputTarget;

pub struct OutputRegistry {
    outputs: HashMap<String, Arc<Mutex<Box<dyn OutputTarget>>>>,
}

impl Default for OutputRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputRegistry {
    pub fn new() -> Self {
        Self {
            outputs: HashMap::new(),
        }
    }

    pub fn register(&mut self, output: Box<dyn OutputTarget>) {
        let id = output.device_id().to_string();
        self.outputs.insert(id, Arc::new(Mutex::new(output)));
    }

    pub fn get(&self, device_id: &str) -> Option<Arc<Mutex<Box<dyn OutputTarget>>>> {
        self.outputs.get(device_id).cloned()
    }

    /// Returns `true` if a device with the given ID is already registered.
    pub fn contains(&self, device_id: &str) -> bool {
        self.outputs.contains_key(device_id)
    }

    pub fn remove(&mut self, device_id: &str) {
        self.outputs.remove(device_id);
    }

    pub fn list(&self) -> Vec<String> {
        self.outputs.keys().cloned().collect()
    }

    pub async fn status_all(&self) -> Vec<serde_json::Value> {
        let mut results = Vec::new();
        for (id, output) in &self.outputs {
            let output = output.lock().await;
            let available = output.is_available().await;
            let mut entry = serde_json::json!({
                "device_id": id,
                "name": output.name(),
                "type": output.output_type(),
                "available": available,
            });
            if let Some(host) = output.host() {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("host".into(), serde_json::json!(host));
            }
            results.push(entry);
        }
        results
    }

    /// Return basic info for every registered output without calling `is_available()`.
    /// This avoids sequential HTTP probes that can block for seconds per device.
    pub async fn info_all(&self) -> Vec<serde_json::Value> {
        let mut results = Vec::new();
        for (id, output) in &self.outputs {
            let output = output.lock().await;
            let mut entry = serde_json::json!({
                "device_id": id,
                "name": output.name(),
                "type": output.output_type(),
            });
            if let Some(host) = output.host() {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("host".into(), serde_json::json!(host));
            }
            results.push(entry);
        }
        results
    }
}
