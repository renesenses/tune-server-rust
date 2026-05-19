use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::OutputTarget;

pub struct OutputRegistry {
    outputs: HashMap<String, Arc<Mutex<Box<dyn OutputTarget>>>>,
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
            results.push(serde_json::json!({
                "device_id": id,
                "name": output.name(),
                "type": output.output_type(),
                "available": available,
            }));
        }
        results
    }
}
