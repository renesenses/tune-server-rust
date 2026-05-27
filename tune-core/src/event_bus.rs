use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// A general-purpose event emitted through the Tune event bus.
///
/// Events use a dotted namespace convention:
///   - `library.scan.started`, `library.scan.progress`, `library.scan.complete`
///   - `library.track.added`
///   - `zone.created`, `zone.deleted`, `zone.updated`
///   - `streaming.auth.success`, `streaming.auth.failed`
///   - `device.discovered`, `device.lost`
///   - `system.restart`, `system.backup.created`
///   - `radio.metadata.updated`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneEvent {
    pub event_type: String,
    pub data: Value,
}

/// Broadcast-based event bus for the entire Tune server.
///
/// Any subsystem can emit events (`emit`) and any number of consumers can
/// subscribe (`subscribe`) to receive a copy.  Dropped events (lag) are
/// silently skipped by consumers.
pub struct EventBus {
    tx: broadcast::Sender<TuneEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    /// Emit an event to all current subscribers.
    pub fn emit(&self, event_type: &str, data: Value) {
        let _ = self.tx.send(TuneEvent {
            event_type: event_type.into(),
            data,
        });
    }

    /// Create a new receiver that will receive all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<TuneEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn emit_and_receive() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();

        bus.emit("library.scan.started", json!({"dirs": ["/music"]}));

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event_type, "library.scan.started");
        assert_eq!(ev.data["dirs"][0], "/music");
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit("zone.created", json!({"id": 1}));

        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.event_type, e2.event_type);
    }

    #[tokio::test]
    async fn no_subscriber_does_not_panic() {
        let bus = EventBus::new();
        bus.emit("system.restart", json!({}));
        // No panic, no error — the event is simply dropped.
    }
}
