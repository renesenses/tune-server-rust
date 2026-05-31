
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::traits::{OutputStatus, TransportState};

const PREBUFFER_THRESHOLD_MS: u64 = 15_000;
const POLL_INTERVAL_MS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
enum GaplessState {
    Idle,
    Monitoring,
    Prebuffering,
    Ready,
}

pub struct GaplessHandler {
    state: Mutex<GaplessState>,
    next_url_set: Mutex<bool>,
    enabled: Mutex<bool>,
    prebuffer_threshold_ms: u64,
}

impl GaplessHandler {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: Mutex::new(GaplessState::Idle),
            next_url_set: Mutex::new(false),
            enabled: Mutex::new(enabled),
            prebuffer_threshold_ms: PREBUFFER_THRESHOLD_MS,
        }
    }

    pub async fn set_enabled(&self, enabled: bool) {
        *self.enabled.lock().await = enabled;
        if !enabled {
            self.reset().await;
        }
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    pub async fn on_play_start(&self) {
        if !*self.enabled.lock().await {
            return;
        }
        *self.state.lock().await = GaplessState::Monitoring;
        *self.next_url_set.lock().await = false;
        debug!("gapless_monitoring_start");
    }

    pub async fn check_prebuffer(
        &self,
        status: &OutputStatus,
        zone_id: i64,
        queue_position: i64,
        queue_length: i64,
        orchestrator: &PlaybackOrchestrator,
        device_id: &str,
    ) -> bool {
        if !*self.enabled.lock().await {
            return false;
        }

        let current_state = self.state.lock().await.clone();
        if current_state != GaplessState::Monitoring {
            return false;
        }

        if status.state != TransportState::Playing || status.duration_ms == 0 {
            return false;
        }

        let remaining_ms = status.duration_ms.saturating_sub(status.position_ms);
        if remaining_ms > self.prebuffer_threshold_ms {
            return false;
        }

        if *self.next_url_set.lock().await {
            return false;
        }

        let next_pos = queue_position + 1;
        if next_pos >= queue_length {
            return false;
        }

        match orchestrator.resolve_queue_item_url(zone_id, next_pos).await {
            Ok(resolved) => {
                let outputs = orchestrator.outputs.lock().await;
                if let Some(output) = outputs.get(device_id) {
                    let out = output.lock().await;
                    match out
                        .set_next_url(
                            &resolved.url,
                            &resolved.mime_type,
                            Some(&resolved.title),
                            resolved.artist.as_deref(),
                        )
                        .await
                    {
                        Ok(()) => {
                            *self.state.lock().await = GaplessState::Ready;
                            *self.next_url_set.lock().await = true;
                            info!(
                                zone_id,
                                next_pos,
                                title = ?resolved.title,
                                "gapless_next_url_set"
                            );
                            return true;
                        }
                        Err(e) => {
                            debug!(error = %e, "gapless_set_next_failed");
                        }
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "gapless_resolve_failed");
            }
        }

        false
    }

    pub async fn on_track_end(&self) {
        let was_ready = *self.state.lock().await == GaplessState::Ready;
        self.reset().await;
        if was_ready {
            info!("gapless_transition_complete");
        }
    }

    pub async fn reset(&self) {
        *self.state.lock().await = GaplessState::Idle;
        *self.next_url_set.lock().await = false;
    }

    pub async fn status(&self) -> serde_json::Value {
        let enabled = *self.enabled.lock().await;
        let state = self.state.lock().await.clone();
        let next_ready = *self.next_url_set.lock().await;
        serde_json::json!({
            "enabled": enabled,
            "state": format!("{state:?}"),
            "next_track_ready": next_ready,
            "prebuffer_threshold_ms": self.prebuffer_threshold_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_disabled() {
        let h = GaplessHandler::new(false);
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn enable_disable() {
        let h = GaplessHandler::new(true);
        assert!(h.is_enabled().await);
        h.set_enabled(false).await;
        assert!(!h.is_enabled().await);
    }

    #[tokio::test]
    async fn monitoring_on_play() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Monitoring);
    }

    #[tokio::test]
    async fn reset_clears_state() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        h.reset().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }

    #[tokio::test]
    async fn status_json() {
        let h = GaplessHandler::new(true);
        h.on_play_start().await;
        let s = h.status().await;
        assert_eq!(s["enabled"], true);
        assert_eq!(s["state"], "Monitoring");
        assert_eq!(s["next_track_ready"], false);
    }

    #[tokio::test]
    async fn no_monitoring_when_disabled() {
        let h = GaplessHandler::new(false);
        h.on_play_start().await;
        let state = h.state.lock().await.clone();
        assert_eq!(state, GaplessState::Idle);
    }
}
