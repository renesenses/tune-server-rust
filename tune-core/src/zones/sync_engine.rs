use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::group::GroupManager;
use crate::outputs::OutputRegistry;
use crate::playback::PlaybackManager;

const DRIFT_FINE_MS: i64 = 50;
const DRIFT_COARSE_MS: i64 = 200;
const POLL_PLAYING_MS: u64 = 100;
const POLL_IDLE_MS: u64 = 5000;
const COOLDOWN_FINE_S: f64 = 1.0;
const COOLDOWN_COARSE_S: f64 = 3.0;
const MAX_DRIFT_HISTORY: usize = 50;

pub struct SyncEngine {
    groups: Arc<Mutex<GroupManager>>,
    outputs: Arc<Mutex<OutputRegistry>>,
    playback: Arc<PlaybackManager>,
    last_correction: Mutex<HashMap<i64, Instant>>,
    drift_history: Mutex<HashMap<i64, Vec<i64>>>,
}

impl SyncEngine {
    pub fn new(
        groups: Arc<Mutex<GroupManager>>,
        outputs: Arc<Mutex<OutputRegistry>>,
        playback: Arc<PlaybackManager>,
    ) -> Self {
        Self {
            groups,
            outputs,
            playback,
            last_correction: Mutex::new(HashMap::new()),
            drift_history: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("sync_engine_started");
            loop {
                let has_active = {
                    let groups = self.groups.lock().await;
                    groups.has_active_groups()
                };
                let interval = if has_active {
                    POLL_PLAYING_MS
                } else {
                    POLL_IDLE_MS
                };

                if has_active
                    && let Err(e) = self.sync_all().await {
                        warn!(error = %e, "sync_engine_error");
                    }

                tokio::time::sleep(std::time::Duration::from_millis(interval)).await;
            }
        })
    }

    async fn sync_all(&self) -> Result<(), String> {
        let groups = self.groups.lock().await;
        let group_infos: Vec<_> = groups.list_groups();
        drop(groups);

        for group_info in &group_infos {
            self.sync_group(group_info).await;
        }
        Ok(())
    }

    async fn sync_group(&self, group_info: &super::group::ZoneGroupInfo) {
        let leader_id = group_info.leader_zone_id;

        let leader_state = self.playback.get_state(leader_id).await;
        if leader_state.state != crate::playback::PlayState::Playing {
            return;
        }
        let leader_pos = leader_state.position_ms;
        if leader_pos <= 0 {
            return;
        }

        // Check settle period
        {
            let groups = self.groups.lock().await;
            if let Some(group) = groups.get_group(&group_info.group_id)
                && let Some(last_play) = group.last_play_time()
                    && last_play.elapsed().as_secs_f64() < COOLDOWN_COARSE_S {
                        return;
                    }
        }

        let outputs = self.outputs.lock().await;
        for &follower_id in &group_info.follower_zone_ids {
            let follower_state = self.playback.get_state(follower_id).await;
            if follower_state.state != crate::playback::PlayState::Playing {
                continue;
            }
            if follower_state.muted {
                continue;
            }
            let follower_pos = follower_state.position_ms;
            if follower_pos <= 0 {
                continue;
            }

            let target = leader_pos;
            let drift = (target - follower_pos).abs();

            // Record drift history
            {
                let mut history = self.drift_history.lock().await;
                let hist = history.entry(follower_id).or_insert_with(Vec::new);
                hist.push(drift);
                if hist.len() > MAX_DRIFT_HISTORY {
                    hist.remove(0);
                }
            }

            if drift > DRIFT_COARSE_MS {
                let should_correct = {
                    let corrections = self.last_correction.lock().await;
                    corrections
                        .get(&follower_id)
                        .map(|t| t.elapsed().as_secs_f64() >= COOLDOWN_COARSE_S)
                        .unwrap_or(true)
                };
                if should_correct
                    && let Some(device_id) = self.get_device_id(follower_id)
                        && let Some(output) = outputs.get(&device_id) {
                            info!(
                                follower = follower_id,
                                drift_ms = drift,
                                target_ms = target,
                                "sync_coarse_correction"
                            );
                            let _ = output.lock().await.seek(target as u64).await;
                            self.last_correction
                                .lock()
                                .await
                                .insert(follower_id, Instant::now());
                        }
            } else if drift > DRIFT_FINE_MS {
                let should_correct = {
                    let corrections = self.last_correction.lock().await;
                    corrections
                        .get(&follower_id)
                        .map(|t| t.elapsed().as_secs_f64() >= COOLDOWN_FINE_S)
                        .unwrap_or(true)
                };
                if should_correct
                    && let Some(device_id) = self.get_device_id(follower_id)
                        && let Some(output) = outputs.get(&device_id) {
                            debug!(
                                follower = follower_id,
                                drift_ms = drift,
                                target_ms = target,
                                "sync_fine_correction"
                            );
                            let _ = output.lock().await.seek(target as u64).await;
                            self.last_correction
                                .lock()
                                .await
                                .insert(follower_id, Instant::now());
                        }
            }
        }
    }

    fn get_device_id(&self, _zone_id: i64) -> Option<String> {
        // TODO: resolve zone_id → output device_id via DB
        None
    }

    pub async fn get_drift_stats(&self) -> HashMap<i64, serde_json::Value> {
        let history = self.drift_history.lock().await;
        let mut stats = HashMap::new();
        for (&zone_id, hist) in history.iter() {
            if hist.is_empty() {
                continue;
            }
            let current = *hist.last().unwrap();
            let avg = hist.iter().sum::<i64>() / hist.len() as i64;
            let max = *hist.iter().max().unwrap();
            stats.insert(
                zone_id,
                serde_json::json!({
                    "current_drift_ms": current,
                    "avg_drift_ms": avg,
                    "max_drift_ms": max,
                    "samples": hist.len(),
                    "in_sync": current < DRIFT_FINE_MS,
                }),
            );
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_thresholds() {
        assert!(DRIFT_FINE_MS < DRIFT_COARSE_MS);
        assert_eq!(DRIFT_FINE_MS, 50);
        assert_eq!(DRIFT_COARSE_MS, 200);
    }

    #[test]
    fn cooldown_ordering() {
        assert!(COOLDOWN_FINE_S < COOLDOWN_COARSE_S);
    }
}
