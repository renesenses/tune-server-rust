use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::TransportState;
use crate::playback::{PlayState, PlaybackManager, RepeatMode};

const POLL_INTERVAL_MS: u64 = 1000;
const GAPLESS_WINDOW_MS: u64 = 10_000;
const STOPPED_TICKS_THRESHOLD: u8 = 2;

struct ZonePollState {
    gapless_sent: bool,
    stopped_ticks: u8,
}

pub struct PositionPoller {
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    db: SqliteDb,
}

impl PositionPoller {
    pub fn new(
        orchestrator: Arc<PlaybackOrchestrator>,
        playback: Arc<PlaybackManager>,
        outputs: Arc<Mutex<OutputRegistry>>,
        db: SqliteDb,
    ) -> Self {
        Self {
            orchestrator,
            playback,
            outputs,
            db,
        }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("position_poller_started");
            let mut ticker = interval(Duration::from_millis(POLL_INTERVAL_MS));
            let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();

            loop {
                ticker.tick().await;
                self.tick(&mut poll_states).await;
            }
        })
    }

    async fn tick(&self, poll_states: &mut HashMap<i64, ZonePollState>) {
        let states = self.playback.all_states().await;

        poll_states.retain(|zone_id, _| {
            states
                .iter()
                .any(|s| s.zone_id == *zone_id && s.state == PlayState::Playing)
        });

        for zone_state in &states {
            if zone_state.state != PlayState::Playing {
                continue;
            }

            let zone_id = zone_state.zone_id;
            let device_id = match self.get_zone_device_id(zone_id) {
                Some(id) => id,
                None => continue,
            };

            let status = {
                let outputs = self.outputs.lock().await;
                let output = match outputs.get(&device_id) {
                    Some(o) => o,
                    None => continue,
                };
                let output = output.lock().await;
                match output.get_status().await {
                    Ok(s) => s,
                    Err(e) => {
                        debug!(zone_id, device = %device_id, error = %e, "poll_failed");
                        continue;
                    }
                }
            };

            self.playback
                .update_position(zone_id, status.position_ms as i64)
                .await;
            self.playback
                .emit_position(zone_id, status.position_ms as i64);

            let ps = poll_states.entry(zone_id).or_insert(ZonePollState {
                gapless_sent: false,
                stopped_ticks: 0,
            });

            match status.state {
                TransportState::Stopped => {
                    ps.stopped_ticks += 1;
                    if ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD {
                        poll_states.remove(&zone_id);
                        self.handle_track_end(zone_id, zone_state).await;
                    }
                }
                TransportState::Playing | TransportState::Transitioning => {
                    ps.stopped_ticks = 0;

                    if !ps.gapless_sent
                        && status.duration_ms > GAPLESS_WINDOW_MS
                        && status.position_ms >= status.duration_ms - GAPLESS_WINDOW_MS
                    {
                        self.prepare_gapless(zone_id, zone_state, &device_id)
                            .await;
                        ps.gapless_sent = true;
                    }
                }
                TransportState::Paused => {
                    ps.stopped_ticks = 0;
                }
            }
        }
    }

    async fn handle_track_end(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
    ) {
        let device_id = self.get_zone_device_id(zone_id);

        let next_pos = match zone_state.repeat {
            RepeatMode::One => zone_state.queue_position,
            RepeatMode::All => {
                if zone_state.queue_length == 0 {
                    self.orchestrator
                        .stop(zone_id, device_id.as_deref())
                        .await;
                    return;
                }
                (zone_state.queue_position + 1) % zone_state.queue_length
            }
            RepeatMode::Off => {
                let next = zone_state.queue_position + 1;
                if next >= zone_state.queue_length {
                    info!(zone_id, "queue_ended");
                    self.orchestrator
                        .stop(zone_id, device_id.as_deref())
                        .await;
                    return;
                }
                next
            }
        };

        info!(zone_id, next_pos, "auto_next");
        if let Err(e) = self.orchestrator.play_from_queue(zone_id, next_pos).await {
            warn!(zone_id, error = %e, "auto_next_failed");
            self.orchestrator
                .stop(zone_id, device_id.as_deref())
                .await;
        }
    }

    async fn prepare_gapless(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: &str,
    ) {
        let next_pos = match zone_state.repeat {
            RepeatMode::One => zone_state.queue_position,
            RepeatMode::All => {
                (zone_state.queue_position + 1) % zone_state.queue_length.max(1)
            }
            RepeatMode::Off => {
                let next = zone_state.queue_position + 1;
                if next >= zone_state.queue_length {
                    return;
                }
                next
            }
        };

        match self
            .orchestrator
            .resolve_queue_item_url(zone_id, next_pos)
            .await
        {
            Ok((url, mime_type, title, artist)) => {
                let outputs = self.outputs.lock().await;
                if let Some(output) = outputs.get(device_id) {
                    let output = output.lock().await;
                    if let Err(e) = output
                        .set_next_url(&url, &mime_type, Some(&title), artist.as_deref())
                        .await
                    {
                        debug!(zone_id, error = %e, "gapless_set_next_failed");
                    } else {
                        info!(zone_id, title = %title, "gapless_next_set");
                    }
                }
            }
            Err(e) => debug!(zone_id, error = %e, "gapless_resolve_failed"),
        }
    }

    fn get_zone_device_id(&self, zone_id: i64) -> Option<String> {
        ZoneRepo::new(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    }
}
