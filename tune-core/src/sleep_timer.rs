use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::info;

use crate::orchestrator::PlaybackOrchestrator;

#[derive(Debug, Clone)]
struct TimerState {
    zone_id: i64,
    device_id: Option<String>,
    end_time: Instant,
    fade_duration_s: u64,
    original_volume: Option<f64>,
}

pub struct SleepTimer {
    active: Mutex<Option<TimerState>>,
    cancel_flag: Mutex<bool>,
}

impl Default for SleepTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl SleepTimer {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(None),
            cancel_flag: Mutex::new(false),
        }
    }

    pub async fn set(
        &self,
        zone_id: i64,
        device_id: Option<String>,
        minutes: u64,
        fade_duration_s: u64,
    ) {
        let end_time = Instant::now() + Duration::from_secs(minutes * 60);
        *self.active.lock().await = Some(TimerState {
            zone_id,
            device_id,
            end_time,
            fade_duration_s,
            original_volume: None,
        });
        *self.cancel_flag.lock().await = false;
        info!(
            zone_id,
            minutes,
            fade_s = fade_duration_s,
            "sleep_timer_set"
        );
    }

    pub async fn cancel(&self) {
        *self.cancel_flag.lock().await = true;
        let timer = self.active.lock().await.take();
        if timer.is_some() {
            info!("sleep_timer_cancelled");
        }
    }

    pub async fn remaining_seconds(&self) -> Option<u64> {
        let timer = self.active.lock().await;
        timer.as_ref().map(|t| {
            let now = Instant::now();
            if now >= t.end_time {
                0
            } else {
                (t.end_time - now).as_secs()
            }
        })
    }

    pub async fn status(&self) -> serde_json::Value {
        let timer = self.active.lock().await;
        match timer.as_ref() {
            Some(t) => {
                let remaining = if Instant::now() >= t.end_time {
                    0
                } else {
                    (t.end_time - Instant::now()).as_secs()
                };
                serde_json::json!({
                    "active": true,
                    "zone_id": t.zone_id,
                    "remaining_seconds": remaining,
                    "fade_duration_s": t.fade_duration_s,
                })
            }
            None => serde_json::json!({ "active": false }),
        }
    }

    pub fn spawn(self: Arc<Self>, orchestrator: Arc<PlaybackOrchestrator>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                if *self.cancel_flag.lock().await {
                    continue;
                }

                let timer = {
                    let guard = self.active.lock().await;
                    match guard.as_ref() {
                        Some(t) => t.clone(),
                        None => continue,
                    }
                };

                let now = Instant::now();
                if now < timer.end_time {
                    let remaining = (timer.end_time - now).as_secs();
                    if remaining <= timer.fade_duration_s && timer.fade_duration_s > 0 {
                        self.fade_step(&timer, remaining, &orchestrator).await;
                    }
                    continue;
                }

                info!(zone_id = timer.zone_id, "sleep_timer_triggered");
                orchestrator
                    .stop(timer.zone_id, timer.device_id.as_deref())
                    .await;

                if let Some(vol) = timer.original_volume {
                    orchestrator
                        .set_volume(timer.zone_id, vol, timer.device_id.as_deref())
                        .await;
                }

                *self.active.lock().await = None;
            }
        });
    }

    async fn fade_step(
        &self,
        timer: &TimerState,
        remaining_s: u64,
        orchestrator: &PlaybackOrchestrator,
    ) {
        let mut guard = self.active.lock().await;
        let state = match guard.as_mut() {
            Some(s) => s,
            None => return,
        };

        if state.original_volume.is_none() {
            let zone_state = orchestrator.playback.get_state(timer.zone_id).await;
            state.original_volume = Some(zone_state.volume);
            info!(
                volume = zone_state.volume,
                fade_s = timer.fade_duration_s,
                "sleep_timer_fade_start"
            );
        }

        if let Some(orig_vol) = state.original_volume {
            let progress = remaining_s as f64 / timer.fade_duration_s as f64;
            let target_vol = orig_vol * progress;
            drop(guard);
            orchestrator
                .set_volume(timer.zone_id, target_vol, timer.device_id.as_deref())
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timer_set_and_status() {
        let timer = SleepTimer::new();
        timer.set(1, None, 30, 60).await;

        let status = timer.status().await;
        assert_eq!(status["active"], true);
        assert_eq!(status["zone_id"], 1);
        assert!(status["remaining_seconds"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn timer_cancel() {
        let timer = SleepTimer::new();
        timer.set(1, None, 10, 0).await;
        timer.cancel().await;

        let status = timer.status().await;
        assert_eq!(status["active"], false);
    }

    #[tokio::test]
    async fn timer_remaining() {
        let timer = SleepTimer::new();
        assert!(timer.remaining_seconds().await.is_none());

        timer.set(1, None, 5, 0).await;
        let rem = timer.remaining_seconds().await.unwrap();
        assert!(rem > 200 && rem <= 300);
    }

    #[tokio::test]
    async fn timer_not_active_by_default() {
        let timer = SleepTimer::new();
        let status = timer.status().await;
        assert_eq!(status["active"], false);
    }
}
