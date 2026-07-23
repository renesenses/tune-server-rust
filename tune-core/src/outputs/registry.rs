use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::OutputTarget;

pub struct OutputRegistry {
    outputs: HashMap<String, Arc<Mutex<Box<dyn OutputTarget>>>>,
    /// device_id → (name, output_type). A synchronous sidecar so callers can
    /// look up an output's name/type without locking its async Mutex (which,
    /// held under the registry lock, would risk a deadlock). `name`/`output_type`
    /// are sync trait methods, so they're captured cheaply at `register` time.
    meta: HashMap<String, (String, String)>,
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
            meta: HashMap::new(),
        }
    }

    pub fn register(&mut self, output: Box<dyn OutputTarget>) {
        let id = output.device_id().to_string();
        self.meta.insert(
            id.clone(),
            (output.name().to_string(), output.output_type().to_string()),
        );
        self.outputs.insert(id, Arc::new(Mutex::new(output)));
    }

    /// device_ids of registered outputs that share `name` (case-insensitive) but
    /// have a different `output_type`. Used to avoid exposing one physical device
    /// as two zones — e.g. a DAC seen both as a Squeezebox player (via an LMS CLI)
    /// and as a DLNA renderer (via that same LMS's UPnP bridge), which is Yacine's
    /// Daphile setup: same name "DENAFRIPS USB HiRes Audio", two protocols.
    pub fn conflicting_outputs(&self, name: &str, own_type: &str) -> Vec<String> {
        self.meta
            .iter()
            .filter(|(_, (n, t))| t != own_type && n.eq_ignore_ascii_case(name))
            .map(|(id, _)| id.clone())
            .collect()
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
        self.meta.remove(device_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::mock::MockOutput;

    #[test]
    fn conflicting_outputs_matches_same_name_across_types() {
        let mut reg = OutputRegistry::new();
        reg.register(Box::new(
            MockOutput::new("dlna-1", "DENAFRIPS USB HiRes Audio").with_type("dlna"),
        ));
        reg.register(Box::new(
            MockOutput::new("squeezebox-mac", "DENAFRIPS USB HiRes Audio").with_type("squeezebox"),
        ));

        // A squeezebox player finds the same-name DLNA output as a conflict
        // (case-insensitive), so it can be skipped to avoid a duplicate zone.
        assert_eq!(
            reg.conflicting_outputs("denafrips usb hires audio", "squeezebox"),
            vec!["dlna-1".to_string()]
        );
        // A different name never conflicts.
        assert!(
            reg.conflicting_outputs("Marantz CINEMA 70s", "squeezebox")
                .is_empty()
        );
        // Same type is not a conflict (never dedup against your own kind).
        assert!(
            reg.conflicting_outputs("DENAFRIPS USB HiRes Audio", "mock")
                .len()
                == 2
        );
        // remove() clears the name index too.
        reg.remove("dlna-1");
        assert!(
            reg.conflicting_outputs("DENAFRIPS USB HiRes Audio", "squeezebox")
                .is_empty()
        );
    }
}
