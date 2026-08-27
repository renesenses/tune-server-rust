use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::traits::OutputTarget;

pub struct OutputRegistry {
    outputs: HashMap<String, Arc<Mutex<Box<dyn OutputTarget>>>>,
    /// device_id → (name, output_type, host). A synchronous sidecar so callers
    /// can look up an output's name/type/host without locking its async Mutex
    /// (which, held under the registry lock, would risk a deadlock).
    /// `name`/`output_type`/`host` are sync trait methods, so they're captured
    /// cheaply at `register` time.
    meta: HashMap<String, (String, String, Option<String>)>,
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
            (
                output.name().to_string(),
                output.output_type().to_string(),
                output.host().map(|h| h.to_string()),
            ),
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
            .filter(|(_, (n, t, _))| t != own_type && n.eq_ignore_ascii_case(name))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Like [`conflicting_outputs`], but also requires the conflicting output to
    /// live on the same `host` (case-insensitive). This is the precise
    /// "genuinely the same physical device seen both ways" test: an LMS
    /// Squeezebox player and that same LMS's UPnP-bridge DLNA renderer report
    /// the same name AND the same host (the LMS box). A different renderer that
    /// merely happens to share a display name is NOT matched, so it is never
    /// wrongly deferred. A conflict whose host is unknown (`None`) is excluded.
    pub fn conflicting_outputs_same_host(
        &self,
        name: &str,
        own_type: &str,
        host: &str,
    ) -> Vec<String> {
        self.meta
            .iter()
            .filter(|(_, (n, t, h))| {
                t != own_type
                    && n.eq_ignore_ascii_case(name)
                    && h.as_deref().is_some_and(|hh| hh.eq_ignore_ascii_case(host))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// device_ids of every registered output whose display name matches `name`
    /// (case-insensitive), paired with its `output_type`.
    ///
    /// Used to recover a zone whose stored `output_device_id` has vanished: the
    /// same physical output may still be present under a different id (e.g. a
    /// Mac's speakers once seen over the network from another server, now only
    /// reachable as a `local:` CoreAudio output). Read from the sync `meta`
    /// sidecar, so no output's async Mutex is locked.
    pub fn find_by_name(&self, name: &str) -> Vec<(String, String)> {
        self.meta
            .iter()
            .filter(|(_, (n, _, _))| n.eq_ignore_ascii_case(name))
            .map(|(id, (_, t, _))| (id.clone(), t.clone()))
            .collect()
    }

    pub fn get(&self, device_id: &str) -> Option<Arc<Mutex<Box<dyn OutputTarget>>>> {
        self.outputs.get(device_id).cloned()
    }

    /// The host recorded for a registered output at `register` time, if any.
    /// Read from the sync `meta` sidecar so callers can compare it without
    /// locking the output's async Mutex.
    pub fn host_of(&self, device_id: &str) -> Option<String> {
        self.meta.get(device_id).and_then(|(_, _, h)| h.clone())
    }

    /// The `output_type` recorded for a registered output at `register` time.
    /// Read from the sync `meta` sidecar (no async lock).
    pub fn type_of(&self, device_id: &str) -> Option<String> {
        self.meta.get(device_id).map(|(_, t, _)| t.clone())
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
                "output_capabilities": output.capabilities(),
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
                "output_capabilities": output.capabilities(),
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

    #[test]
    fn find_by_name_is_case_insensitive_and_carries_the_type() {
        let mut reg = OutputRegistry::new();
        reg.register(Box::new(
            MockOutput::new("local:mac-speakers", "Mac Studio Speakers").with_type("local"),
        ));
        reg.register(Box::new(
            MockOutput::new("dlna-1", "Marantz CINEMA 70s").with_type("dlna"),
        ));

        let found = reg.find_by_name("mac studio speakers");
        assert_eq!(
            found,
            vec![("local:mac-speakers".to_string(), "local".to_string())]
        );
        assert!(reg.find_by_name("Zone fantôme").is_empty());

        // remove() purge aussi l'index de noms.
        reg.remove("local:mac-speakers");
        assert!(reg.find_by_name("Mac Studio Speakers").is_empty());
    }

    #[test]
    fn conflicting_outputs_same_host_requires_matching_host() {
        let mut reg = OutputRegistry::new();
        // A DLNA renderer bridged by the LMS box at 192.168.1.20.
        reg.register(Box::new(
            MockOutput::new("dlna-lms", "DENAFRIPS USB HiRes Audio")
                .with_type("dlna")
                .with_host("192.168.1.20"),
        ));
        // A DIFFERENT DLNA renderer that merely shares the display name, on
        // another box — must NOT be treated as the same device.
        reg.register(Box::new(
            MockOutput::new("dlna-other", "DENAFRIPS USB HiRes Audio")
                .with_type("dlna")
                .with_host("192.168.1.99"),
        ));

        // The Squeezebox player served by the same LMS box (192.168.1.20)
        // matches only the co-hosted DLNA renderer.
        assert_eq!(
            reg.conflicting_outputs_same_host(
                "denafrips usb hires audio",
                "squeezebox",
                "192.168.1.20"
            ),
            vec!["dlna-lms".to_string()]
        );
        // A conflict with an unknown host is excluded.
        reg.register(Box::new(
            MockOutput::new("dlna-nohost", "DENAFRIPS USB HiRes Audio").with_type("dlna"),
        ));
        assert_eq!(
            reg.conflicting_outputs_same_host(
                "DENAFRIPS USB HiRes Audio",
                "squeezebox",
                "192.168.1.20"
            ),
            vec!["dlna-lms".to_string()]
        );
    }
}
