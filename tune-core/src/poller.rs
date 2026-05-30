use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tokio::time::{Duration, interval};
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
const RADIO_POLL_INTERVAL_SECS: u64 = 15;

fn rand_pos(queue_length: i64, current: i64) -> i64 {
    if queue_length <= 1 {
        return 0;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as i64;
    let pos = seed.abs() % queue_length;
    if pos == current {
        (pos + 1) % queue_length
    } else {
        pos
    }
}

struct ZonePollState {
    gapless_sent: bool,
    stopped_ticks: u8,
    last_radio_poll: Instant,
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

        // Also poll stopped zones to detect externally-started playback and sync volume
        let all_zones = crate::db::zone_repo::ZoneRepo::new(self.db.clone())
            .list()
            .unwrap_or_default();

        for zone in &all_zones {
            let zone_id = zone.id.unwrap_or(0);
            if zone_id == 0 {
                continue;
            }
            let device_id = match zone.output_device_id.as_deref() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };

            let in_states = states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);
            if in_states {
                continue;
            } // already handled below

            let status = {
                let outputs = self.outputs.lock().await;
                let output = match outputs.get(&device_id) {
                    Some(o) => o,
                    None => continue,
                };
                let output = output.lock().await;
                match output.get_status().await {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            };

            // Sync volume from device regardless of state
            if status.volume > 0.001 {
                self.playback.set_volume(zone_id, status.volume).await;
                let vol_int = (status.volume * 100.0) as i32;
                crate::db::zone_repo::ZoneRepo::new(self.db.clone())
                    .update_volume(zone_id, vol_int)
                    .ok();
            }

            // Recover playing state from device (only if not already playing in memory)
            let already_playing = states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);
            if status.state == TransportState::Playing && !already_playing {
                let np = crate::playback::NowPlaying {
                    track_id: None,
                    title: status.track_title.unwrap_or_else(|| "Recovering...".into()),
                    artist_name: status.track_artist,
                    album_title: None,
                    cover_path: None,
                    duration_ms: status.duration_ms as i64,
                    source: "local".into(),
                    source_id: None,
                    stream_id: None,
                };
                self.playback.play(zone_id, np).await;
                info!(zone_id, device = %device_id, "playback_recovered_from_device");
            }
        }

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
            if (status.volume - zone_state.volume).abs() > 0.005 {
                self.playback.set_volume(zone_id, status.volume).await;
                let vol_int = (status.volume * 100.0) as i32;
                let db = self.db.clone();
                crate::db::zone_repo::ZoneRepo::new(db)
                    .update_volume(zone_id, vol_int)
                    .ok();
            }
            self.playback
                .emit_position(zone_id, status.position_ms as i64);

            // --- Radio metadata polling ---
            let is_radio = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.source == "radio")
                .unwrap_or(false);
            if is_radio {
                let should_poll = poll_states
                    .get(&zone_id)
                    .map(|ps| {
                        ps.last_radio_poll.elapsed()
                            > std::time::Duration::from_secs(RADIO_POLL_INTERVAL_SECS)
                    })
                    .unwrap_or(true);

                #[allow(clippy::collapsible_if)]
                if should_poll {
                    if let Some(ref np) = zone_state.now_playing {
                        if let Some(ref source_id) = np.source_id {
                            if let Ok(sid) = source_id.parse::<i64>() {
                                let radio_repo =
                                    crate::db::radio_repo::RadioRepo::new(self.db.clone());
                                if let Ok(Some(station)) = radio_repo.get(sid) {
                                    if let Some(meta) = crate::radio_metadata::fetch_radio_metadata(
                                        &station.name,
                                        &station.url,
                                    )
                                    .await
                                    {
                                        // Only update if the metadata actually changed
                                        let title_changed =
                                            np.title != meta.title || np.artist_name != meta.artist;
                                        if title_changed {
                                            let new_np = crate::playback::NowPlaying {
                                                track_id: None,
                                                title: meta.title,
                                                artist_name: meta.artist,
                                                album_title: Some(station.name.clone()),
                                                cover_path: np.cover_path.clone(),
                                                duration_ms: 0,
                                                source: "radio".into(),
                                                source_id: np.source_id.clone(),
                                                stream_id: np.stream_id.clone(),
                                            };
                                            self.playback.update_now_playing(zone_id, new_np).await;
                                            debug!(zone_id, station = %station.name, "radio_metadata_updated");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Update the timestamp (or create entry if missing)
                    if let Some(ps) = poll_states.get_mut(&zone_id) {
                        ps.last_radio_poll = Instant::now();
                    }
                }
            }

            let ps = poll_states.entry(zone_id).or_insert(ZonePollState {
                gapless_sent: false,
                stopped_ticks: 0,
                last_radio_poll: Instant::now(),
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

                    let track_duration = zone_state
                        .now_playing
                        .as_ref()
                        .map(|np| np.duration_ms as u64)
                        .unwrap_or(0);

                    // Detect gapless transition: renderer reports a different duration
                    // than the current track, meaning it moved to the next track.
                    // Use 2s tolerance to avoid false positives from rounding (WAV).
                    // Ignore if renderer reports duration 0 (no metadata).
                    let duration_changed = ps.gapless_sent
                        && track_duration > 0
                        && status.duration_ms > 0
                        && (status.duration_ms as i64 - track_duration as i64).unsigned_abs() > 2000;
                    if duration_changed {
                        info!(zone_id, renderer_dur = status.duration_ms, track_dur = track_duration, "gapless_transition_detected");
                        ps.gapless_sent = false;
                        self.handle_track_end(zone_id, zone_state).await;
                    } else if !ps.gapless_sent
                        && status.duration_ms > GAPLESS_WINDOW_MS
                        && status.position_ms >= status.duration_ms - GAPLESS_WINDOW_MS
                    {
                        // Only send SetNextAVTransportURI if gapless is enabled for this zone
                        let gapless_enabled = ZoneRepo::new(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .map(|z| z.gapless_enabled)
                            .unwrap_or(true);
                        if gapless_enabled {
                            self.prepare_gapless(zone_id, zone_state, &device_id).await;
                        } else {
                            debug!(zone_id, "gapless_disabled_for_zone");
                        }
                        ps.gapless_sent = true;
                    }
                }
                TransportState::Paused => {
                    ps.stopped_ticks = 0;
                }
            }
        }
    }

    fn next_position(zone_state: &crate::playback::ZoneState) -> Option<i64> {
        if zone_state.queue_length == 0 {
            return None;
        }
        match zone_state.repeat {
            RepeatMode::One => Some(zone_state.queue_position),
            RepeatMode::All => {
                if zone_state.shuffle {
                    Some(rand_pos(zone_state.queue_length, zone_state.queue_position))
                } else {
                    Some((zone_state.queue_position + 1) % zone_state.queue_length)
                }
            }
            RepeatMode::Off => {
                if zone_state.shuffle {
                    Some(rand_pos(zone_state.queue_length, zone_state.queue_position))
                } else {
                    let next = zone_state.queue_position + 1;
                    if next >= zone_state.queue_length {
                        None
                    } else {
                        Some(next)
                    }
                }
            }
        }
    }

    async fn handle_track_end(&self, zone_id: i64, zone_state: &crate::playback::ZoneState) {
        let device_id = self.get_zone_device_id(zone_id);

        let Some(next_pos) = Self::next_position(zone_state) else {
            info!(zone_id, "queue_ended");
            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
            return;
        };

        info!(zone_id, next_pos, "auto_next");
        if let Err(e) = self.orchestrator.play_from_queue(zone_id, next_pos).await {
            warn!(zone_id, error = %e, "auto_next_failed");
            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
        }
    }

    async fn prepare_gapless(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: &str,
    ) {
        let Some(next_pos) = Self::next_position(zone_state) else {
            return;
        };

        match self
            .orchestrator
            .resolve_queue_item_url(zone_id, next_pos)
            .await
        {
            Ok(resolved) => {
                let outputs = self.outputs.lock().await;
                if let Some(output) = outputs.get(device_id) {
                    let output = output.lock().await;
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                    };
                    if let Err(e) = output.set_next_media(&media).await {
                        debug!(zone_id, error = %e, "gapless_set_next_failed");
                    } else {
                        info!(zone_id, title = %resolved.title, "gapless_next_set");
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
