use std::collections::HashMap;
use std::time::Instant;

use tracing::info;

const DEFAULT_BUFFER_S: f64 = 2.0;
const MIN_BUFFER_S: f64 = 1.0;
const MAX_BUFFER_S: f64 = 10.0;
const BUFFER_STEP_S: f64 = 1.0;
const UNDERRUN_THRESHOLD: usize = 3;
const UNDERRUN_WINDOW_S: f64 = 600.0;
const STABILITY_WINDOW_S: f64 = 1800.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Disconnection,
    Underrun,
    Interruption,
}

struct StabilityEvent {
    _kind: EventKind,
    timestamp: Instant,
}

pub struct DeviceBufferStats {
    pub device_id: String,
    pub device_name: String,
    pub buffer_s: f64,
    pub manual_override: bool,
    pub total_disconnections: u64,
    pub total_underruns: u64,
    pub total_interruptions: u64,
    pub total_adjustments: u64,
    recent_events: Vec<StabilityEvent>,
    last_stable_check: Instant,
}

impl DeviceBufferStats {
    pub fn new(device_id: String, device_name: String) -> Self {
        Self {
            device_id,
            device_name,
            buffer_s: DEFAULT_BUFFER_S,
            manual_override: false,
            total_disconnections: 0,
            total_underruns: 0,
            total_interruptions: 0,
            total_adjustments: 0,
            recent_events: Vec::new(),
            last_stable_check: Instant::now(),
        }
    }

    pub fn record_event(&mut self, kind: EventKind) -> Option<f64> {
        match kind {
            EventKind::Disconnection => self.total_disconnections += 1,
            EventKind::Underrun => self.total_underruns += 1,
            EventKind::Interruption => self.total_interruptions += 1,
        }
        self.recent_events.push(StabilityEvent {
            _kind: kind,
            timestamp: Instant::now(),
        });
        self.prune_old_events(UNDERRUN_WINDOW_S);
        if self.manual_override {
            return None;
        }
        self.maybe_increase_buffer()
    }

    fn prune_old_events(&mut self, window_s: f64) {
        let now = Instant::now();
        self.recent_events
            .retain(|e| now.duration_since(e.timestamp).as_secs_f64() < window_s);
    }

    fn maybe_increase_buffer(&mut self) -> Option<f64> {
        let count = self.recent_events.len();
        if count >= UNDERRUN_THRESHOLD && self.buffer_s < MAX_BUFFER_S {
            let old = self.buffer_s;
            self.buffer_s = (self.buffer_s + BUFFER_STEP_S).min(MAX_BUFFER_S);
            self.total_adjustments += 1;
            self.last_stable_check = Instant::now();
            info!(
                device = %self.device_name,
                old_buffer = old,
                new_buffer = self.buffer_s,
                events = count,
                "dlna_buffer_increased"
            );
            Some(self.buffer_s)
        } else {
            None
        }
    }

    pub fn check_stability_decrease(&mut self) -> Option<f64> {
        if self.manual_override || self.buffer_s <= DEFAULT_BUFFER_S {
            return None;
        }
        self.prune_old_events(STABILITY_WINDOW_S);
        let elapsed = self.last_stable_check.elapsed().as_secs_f64();
        if self.recent_events.is_empty() && elapsed >= STABILITY_WINDOW_S {
            let old = self.buffer_s;
            self.buffer_s = (self.buffer_s - BUFFER_STEP_S).max(DEFAULT_BUFFER_S);
            self.last_stable_check = Instant::now();
            self.total_adjustments += 1;
            info!(
                device = %self.device_name,
                old_buffer = old,
                new_buffer = self.buffer_s,
                "dlna_buffer_decreased_stable"
            );
            Some(self.buffer_s)
        } else {
            None
        }
    }

    pub fn set_manual_buffer(&mut self, buffer_s: f64) {
        let clamped = buffer_s.clamp(MIN_BUFFER_S, MAX_BUFFER_S);
        info!(
            device = %self.device_name,
            old = self.buffer_s,
            new = clamped,
            "dlna_buffer_manual_set"
        );
        self.buffer_s = clamped;
        self.manual_override = true;
    }

    pub fn clear_manual_override(&mut self) {
        self.manual_override = false;
        info!(device = %self.device_name, "dlna_buffer_manual_cleared");
    }

    pub fn to_json(&self) -> serde_json::Value {
        let now = Instant::now();
        let recent_count = self
            .recent_events
            .iter()
            .filter(|e| now.duration_since(e.timestamp).as_secs_f64() < UNDERRUN_WINDOW_S)
            .count();
        serde_json::json!({
            "device_id": self.device_id,
            "device_name": self.device_name,
            "buffer_s": self.buffer_s,
            "manual_override": self.manual_override,
            "total_disconnections": self.total_disconnections,
            "total_underruns": self.total_underruns,
            "total_interruptions": self.total_interruptions,
            "total_adjustments": self.total_adjustments,
            "recent_events_in_window": recent_count,
            "window_minutes": (UNDERRUN_WINDOW_S / 60.0) as u32,
        })
    }
}

pub struct DlnaBufferStatsRegistry {
    devices: HashMap<String, DeviceBufferStats>,
}

impl DlnaBufferStatsRegistry {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub fn get_or_create(&mut self, device_id: &str, device_name: &str) -> &mut DeviceBufferStats {
        self.devices
            .entry(device_id.to_string())
            .and_modify(|d| {
                if !device_name.is_empty() && d.device_name.is_empty() {
                    d.device_name = device_name.to_string();
                }
            })
            .or_insert_with(|| {
                DeviceBufferStats::new(device_id.to_string(), device_name.to_string())
            })
    }

    pub fn record_event(
        &mut self,
        device_id: &str,
        kind: EventKind,
        device_name: &str,
    ) -> Option<f64> {
        let stats = self.get_or_create(device_id, device_name);
        stats.record_event(kind)
    }

    pub fn get_buffer_s(&self, device_id: &str) -> f64 {
        self.devices
            .get(device_id)
            .map(|s| s.buffer_s)
            .unwrap_or(DEFAULT_BUFFER_S)
    }

    pub fn all_stats(&self) -> Vec<serde_json::Value> {
        self.devices.values().map(|s| s.to_json()).collect()
    }

    pub fn check_all_stability(&mut self) {
        for stats in self.devices.values_mut() {
            stats.check_stability_decrease();
        }
    }
}

impl Default for DlnaBufferStatsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_buffer() {
        let reg = DlnaBufferStatsRegistry::new();
        assert_eq!(reg.get_buffer_s("unknown"), DEFAULT_BUFFER_S);
    }

    #[test]
    fn buffer_increases_after_threshold() {
        let mut stats = DeviceBufferStats::new("dev1".into(), "Test".into());
        assert_eq!(stats.buffer_s, 2.0);
        stats.record_event(EventKind::Underrun);
        stats.record_event(EventKind::Underrun);
        let result = stats.record_event(EventKind::Underrun);
        assert_eq!(result, Some(3.0));
        assert_eq!(stats.buffer_s, 3.0);
    }

    #[test]
    fn manual_override_prevents_auto() {
        let mut stats = DeviceBufferStats::new("dev1".into(), "Test".into());
        stats.set_manual_buffer(5.0);
        assert!(stats.manual_override);
        for _ in 0..5 {
            assert!(stats.record_event(EventKind::Underrun).is_none());
        }
        assert_eq!(stats.buffer_s, 5.0);
    }

    #[test]
    fn buffer_clamped() {
        let mut stats = DeviceBufferStats::new("dev1".into(), "Test".into());
        stats.set_manual_buffer(20.0);
        assert_eq!(stats.buffer_s, MAX_BUFFER_S);
        stats.set_manual_buffer(0.1);
        assert_eq!(stats.buffer_s, MIN_BUFFER_S);
    }

    #[test]
    fn registry_get_or_create() {
        let mut reg = DlnaBufferStatsRegistry::new();
        reg.get_or_create("d1", "Device 1");
        reg.get_or_create("d1", "");
        assert_eq!(reg.all_stats().len(), 1);
        assert_eq!(
            reg.all_stats()[0]["device_name"].as_str().unwrap(),
            "Device 1"
        );
    }

    #[test]
    fn to_json_fields() {
        let stats = DeviceBufferStats::new("dev1".into(), "Test Device".into());
        let json = stats.to_json();
        assert_eq!(json["device_id"], "dev1");
        assert_eq!(json["buffer_s"], 2.0);
        assert_eq!(json["manual_override"], false);
    }
}
