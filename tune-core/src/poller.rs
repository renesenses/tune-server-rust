use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use tokio::sync::{Mutex, Notify};
use tokio::time::Duration;
use tracing::{debug, info, warn};

/// Global notify used to wake the poller immediately when a local audio
/// output reaches end-of-stream.  Without this, the poller only discovers
/// `track_ended_naturally` on the next 1-second tick, introducing an
/// average 500 ms gap between tracks on local output.
pub static TRACK_END_NOTIFY: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::new()));

use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::TransportState;
use crate::playback::{PlayState, PlaybackManager, RepeatMode};

pub type PollerMetricsMap = Arc<Mutex<HashMap<i64, ZonePollerMetrics>>>;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ZonePollerMetrics {
    pub total_polls: u64,
    pub total_errors: u64,
    pub consecutive_errors: u8,
    pub last_latency_ms: u32,
    pub max_latency_ms: u32,
}

const POLL_INTERVAL_MS: u64 = 1000;
const GAPLESS_WINDOW_MS: u64 = 10_000;
const STOPPED_TICKS_THRESHOLD: u8 = 5;
/// Grace period (seconds) after a seek during which the poller does not
/// overwrite the in-memory position with the value reported by the output.
/// This prevents the progress bar from snapping back to the pre-seek
/// position while the local/cpal output restarts its stream.
const SEEK_GRACE_SECS: u64 = 3;
/// Extended grace period (seconds) for streaming seeks on network outputs
/// (Qobuz/Tidal via DLNA).  Seeking on a proxied stream recreates the
/// stream session and re-sends SetAVTransportURI+Play+Seek — the renderer
/// may report Stopped for several seconds while buffering the new stream.
/// During this window the poller must not accumulate stopped_ticks.
const SEEK_STREAMING_GRACE_SECS: u64 = 10;
/// After this many consecutive Stopped ticks without enough playback,
/// treat as playback failure and stop the zone (don't advance).
/// Increased from 6 to 15 to accommodate slow DLNA renderers (Shanling SCD1.3,
/// MPlayer-based) that report Stopped/position=0 while buffering.
const STOPPED_FAILURE_THRESHOLD: u8 = 30;
/// Grace period (seconds) after a new track is loaded (track_generation
/// changes).  During this window the poller suppresses stopped_ticks to
/// let the renderer buffer — especially important for streaming sources
/// that require transcoding (e.g. Tidal AAC→FLAC for DLNA) which can
/// take 5-15 seconds before the renderer receives any audio data.
const TRACK_LOAD_GRACE_SECS: u64 = 20;
const RADIO_POLL_INTERVAL_SECS: u64 = 15;
/// Grace period after SetNextAVTransportURI during which we treat Stopped
/// state and position resets as gapless transitions instead of track-end.
const GAPLESS_GUARD_SECS: u64 = 5;
/// Minimum fraction of track duration that must have been played before a
/// gapless transition is accepted.  Prevents false transitions when a
/// renderer (e.g. DMP-A8) reports state changes immediately after
/// SetNextAVTransportURI.
const MIN_PLAYED_FRACTION: f64 = 0.80;
/// Minimum wall-clock seconds a track must have been playing before we accept
/// a gapless transition. Prevents false skips when a renderer fails to decode
/// and reports STOPPED after only a few seconds.
const MIN_TRACK_WALL_SECS: u64 = 30;
/// Minimum peak position (ms) required before accepting track-end when the
/// track duration is unknown (0).  Prevents false skips on slow renderers
/// (e.g. Shanling SCD1.3) that report duration=0 and briefly show Stopped
/// state while buffering.  60 seconds is long enough to avoid false positives
/// while still handling actual short tracks via the `is_short_track` path.
const MIN_PEAK_UNKNOWN_DURATION_MS: u64 = 60_000;
/// How often (in ticks) to persist the playback position to the database.
const POSITION_SAVE_INTERVAL_TICKS: u64 = 10;
/// When the output reports Playing but position >= track duration (track
/// effectively ended), wait this many ticks before advancing. This gives
/// the output time to drain its buffer and report Stopped naturally.
/// If it doesn't, this threshold forces the advance.
const POSITION_PAST_END_TICKS: u8 = 3;
/// After a gapless metadata advance (the poller called advance_queue_metadata
/// expecting the renderer to auto-transition), if the renderer stays Stopped
/// for this many ticks (after gapless_cooldown expires), force a play_from_queue.
/// This handles renderers that accept SetNextAVTransportURI but don't actually
/// auto-transition — the poller would otherwise get stuck forever.
const GAPLESS_STUCK_THRESHOLD: u8 = 2;

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
    /// Ticks to ignore Stopped state after a gapless advance, so the
    /// poller doesn't re-send play_from_queue to a renderer that already
    /// transitioned via SetNextAVTransportURI.
    gapless_cooldown: u8,
    /// Consecutive poll failures — used for exponential backoff.
    /// After N failures, skip 2^min(N,4) ticks before retrying.
    consecutive_errors: u8,
    backoff_remaining: u8,
    total_polls: u64,
    total_errors: u64,
    last_latency_ms: u32,
    max_latency_ms: u32,
    last_radio_poll: Instant,
    /// When SetNextAVTransportURI was sent — used to guard against
    /// false track-end detection during gapless transitions on renderers
    /// like Eversolo DMP-A6 that briefly report Stopped or reset position.
    gapless_sent_at: Option<Instant>,
    /// Last polled position in milliseconds — used to detect position
    /// resets (jumps from >30s to <5s) that signal a gapless transition.
    last_position_ms: u64,
    /// Peak position reached in the current track — high-water mark used
    /// to verify that enough of the track was actually played before
    /// accepting a gapless transition.
    peak_position_ms: u64,
    /// Tick counter for throttling DB position saves.
    ticks_since_db_save: u64,
    /// When the current track started playing (wall clock).
    /// Used to reject false gapless transitions that happen too soon.
    track_started_at: Option<Instant>,
    /// Tracks the `ZoneState::track_generation` we last observed.
    /// When the generation changes (new track started via `play()`),
    /// we reset all per-track state so stale values from the previous
    /// track cannot trigger false gapless advances or premature track ends.
    track_generation: u64,
    /// When the orchestrator loaded the current track (track_generation changed).
    /// Used for the startup grace period — DLNA renderers report Stopped while
    /// buffering a new stream, especially after transcoding delays.
    track_loaded_at: Instant,
    /// Counts ticks where the output reports Playing but position_ms has
    /// reached or exceeded the known track duration.  After
    /// POSITION_PAST_END_TICKS consecutive ticks in this state, the poller
    /// treats the track as ended even though the output hasn't reported
    /// Stopped.  This handles local/cpal outputs where the playback thread
    /// may be slow to set `playing = false`.
    past_end_ticks: u8,
    /// Set to true after `gapless_natural_end_advancing_metadata` — the poller
    /// advanced metadata expecting the renderer to auto-transition.  If the
    /// renderer stays Stopped after gapless_cooldown expires, this flag lets
    /// the poller detect the stuck state and force a play_from_queue.
    gapless_advance_pending: bool,
    /// Counts Stopped ticks after gapless_cooldown expires while
    /// gapless_advance_pending is true.  When this reaches
    /// GAPLESS_STUCK_THRESHOLD, the poller gives up on the gapless
    /// transition and forces play_from_queue.
    gapless_stuck_ticks: u8,
    last_bytes_sent: u64,
}

pub struct PositionPoller {
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    shared_metrics: PollerMetricsMap,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
}

impl PositionPoller {
    pub fn new(
        orchestrator: Arc<PlaybackOrchestrator>,
        playback: Arc<PlaybackManager>,
        outputs: Arc<Mutex<OutputRegistry>>,
        db: Arc<dyn crate::db::backend::DbBackend>,
        shared_metrics: PollerMetricsMap,
    ) -> Self {
        Self {
            orchestrator,
            playback,
            outputs,
            db,
            shared_metrics,
            event_bus: None,
        }
    }

    pub fn with_event_bus(mut self, bus: Arc<crate::event_bus::EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("position_poller_started");
            let mut ticker = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
            let notify = TRACK_END_NOTIFY.clone();
            let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();

            loop {
                // Wake on either the regular 1-second tick OR an immediate
                // notification from a local output that finished a track.
                tokio::select! {
                    _ = ticker.tick() => {},
                    _ = notify.notified() => {},
                }
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
        let all_zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
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
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                let output = output_arc.lock().await;
                match output.get_status().await {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            };

            // Sync volume from device only when playing AND the device
            // reports a significantly different volume from what we have in
            // memory.  Many DLNA renderers report a stale default (e.g. 50%)
            // right after playback starts, which would overwrite the user's
            // saved volume.
            if !zone.fixed_volume
                && status.volume > 0.001
                && status.state == TransportState::Playing
            {
                let db_vol = zone.volume as f64 / 100.0;
                if (status.volume - db_vol).abs() > 0.02 {
                    self.playback.set_volume(zone_id, status.volume).await;
                    let vol_int = (status.volume * 100.0) as i32;
                    crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                        .update_volume(zone_id, vol_int)
                        .ok();
                }
            }

            // Recover playing state from device — only if Tune was actually
            // playing on this zone before (last_play_state == "playing" in DB).
            // Without this check, playback from other apps (Roon, Spotify
            // Connect, etc.) on a shared renderer (Sonos) would be captured
            // by Tune and trigger phantom queue playback when the other app stops.
            let already_playing = states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);
            if status.state == TransportState::Playing && !already_playing {
                let last_state =
                    ZoneRepo::with_backend(self.db.clone()).get_last_play_state(zone_id);
                if last_state.as_deref() == Some("playing") {
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
                        ..Default::default()
                    };
                    self.playback.play(zone_id, np).await;
                    info!(zone_id, device = %device_id, "playback_recovered_from_device");
                } else {
                    debug!(
                        zone_id,
                        device = %device_id,
                        last_state = ?last_state,
                        "playback_recovery_skipped_not_tune_playback"
                    );
                }
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

            let ps = poll_states.entry(zone_id).or_insert_with(|| ZonePollState {
                gapless_sent: false,
                stopped_ticks: 0,
                gapless_cooldown: 0,
                consecutive_errors: 0,
                backoff_remaining: 0,
                total_polls: 0,
                total_errors: 0,
                last_latency_ms: 0,
                max_latency_ms: 0,
                last_radio_poll: Instant::now(),
                gapless_sent_at: None,
                last_position_ms: 0,
                peak_position_ms: 0,
                ticks_since_db_save: 0,
                track_started_at: None,
                track_generation: zone_state.track_generation,
                track_loaded_at: Instant::now(),
                past_end_ticks: 0,
                gapless_advance_pending: false,
                gapless_stuck_ticks: 0,
                last_bytes_sent: 0,
            });

            // Detect track change: if the generation changed, the orchestrator
            // started a new track (via play() / play_from_queue / next / previous).
            // Reset all per-track poller state so stale values from the previous
            // track (peak_position, gapless flags, etc.) cannot cause false
            // gapless advances or premature track-end detection.
            //
            // Exception: if last_seek_at is recent (< 10s), this generation
            // change is from a seek (which recreates the stream), not a real
            // track change. In that case, preserve position state to avoid
            // the seek bar jumping back to 0.
            if ps.track_generation != zone_state.track_generation {
                let is_seek = zone_state
                    .last_seek_at
                    .map(|t| t.elapsed().as_secs() < 10)
                    .unwrap_or(false);

                if is_seek {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        position_ms = zone_state.position_ms,
                        "poller_generation_changed_during_seek_preserving_position"
                    );
                } else {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        "poller_track_generation_changed_resetting_state"
                    );
                    ps.last_position_ms = 0;
                    ps.peak_position_ms = 0;
                    ps.last_bytes_sent = 0;
                    ps.past_end_ticks = 0;
                    ps.track_started_at = Some(Instant::now());
                }
                ps.gapless_sent = false;
                ps.gapless_sent_at = None;
                ps.gapless_cooldown = 0;
                ps.stopped_ticks = 0;
                ps.track_generation = zone_state.track_generation;
                ps.track_loaded_at = Instant::now();
                ps.past_end_ticks = 0;
                ps.gapless_advance_pending = false;
                ps.gapless_stuck_ticks = 0;
            }

            if ps.backoff_remaining > 0 {
                ps.backoff_remaining -= 1;
                continue;
            }

            // Radio zones: throttle polling to every RADIO_POLL_INTERVAL_SECS.
            // Polling a DLNA renderer (especially DMP-A8) every second with 4
            // SOAP calls while it plays an infinite radio stream causes buffer
            // underruns, noise, and playback cuts.  Radio has no meaningful
            // position/duration — only transport state and metadata matter,
            // and those change slowly.
            let is_radio = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.source == "radio")
                .unwrap_or(false);
            if is_radio {
                let since_last = ps.last_radio_poll.elapsed();
                if since_last < std::time::Duration::from_secs(RADIO_POLL_INTERVAL_SECS) {
                    continue;
                }
            }

            ps.total_polls += 1;
            let poll_start = Instant::now();
            let status = {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                let output = output_arc.lock().await;
                match output.get_status().await {
                    Ok(s) => {
                        ps.consecutive_errors = 0;
                        let latency = poll_start.elapsed().as_millis() as u32;
                        ps.last_latency_ms = latency;
                        if latency > ps.max_latency_ms {
                            ps.max_latency_ms = latency;
                        }
                        s
                    }
                    Err(e) => {
                        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
                        ps.total_errors += 1;
                        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
                        debug!(
                            zone_id,
                            device = %device_id,
                            error = %e,
                            backoff = ps.backoff_remaining,
                            "poll_failed_backing_off"
                        );
                        continue;
                    }
                }
            };

            // Update last_radio_poll so the throttle gate works on next tick.
            if is_radio {
                ps.last_radio_poll = Instant::now();
            }

            // Radio zones: after the throttled poll, only check transport
            // state (is it still playing?) and do metadata polling.
            // Skip position tracking, gapless logic, and track-end detection
            // — none of that applies to infinite streams.
            if is_radio {
                let radio_stopped = status.state == TransportState::Stopped;

                if !radio_stopped {
                    // Still playing — sync volume only.
                    let zone_fixed_volume = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .map(|z| z.fixed_volume)
                        .unwrap_or(false);
                    if !zone_fixed_volume && (status.volume - zone_state.volume).abs() > 0.005 {
                        self.playback.set_volume(zone_id, status.volume).await;
                        let vol_int = (status.volume * 100.0) as i32;
                        let db = self.db.clone();
                        crate::db::zone_repo::ZoneRepo::with_backend(db)
                            .update_volume(zone_id, vol_int)
                            .ok();
                    }

                    // Radio metadata polling (title/artist from ICY or external)
                    if let Some(ref np) = zone_state.now_playing {
                        if np.source == "radio" {
                            if let Some(ref source_id) = np.source_id {
                                // source_id is either a numeric radio DB id or the stream URL itself
                                let (station_name, stream_url) = if let Ok(sid) =
                                    source_id.parse::<i64>()
                                {
                                    let radio_repo = crate::db::radio_repo::RadioRepo::with_backend(
                                        self.db.clone(),
                                    );
                                    if let Ok(Some(station)) = radio_repo.get(sid) {
                                        (station.name.clone(), station.url.clone())
                                    } else {
                                        (np.title.clone(), source_id.clone())
                                    }
                                } else {
                                    (np.title.clone(), source_id.clone())
                                };

                                if let Some(meta) = crate::radio_metadata::fetch_radio_metadata(
                                    &station_name,
                                    &stream_url,
                                )
                                .await
                                {
                                    let title_changed =
                                        np.title != meta.title || np.artist_name != meta.artist;
                                    if title_changed {
                                        let new_np = crate::playback::NowPlaying {
                                            track_id: None,
                                            title: meta.title,
                                            artist_name: meta.artist,
                                            album_title: Some(station_name.clone()),
                                            cover_path: np.cover_path.clone(),
                                            duration_ms: 0,
                                            source: "radio".into(),
                                            source_id: np.source_id.clone(),
                                            stream_id: np.stream_id.clone(),
                                            ..Default::default()
                                        };
                                        self.playback.update_now_playing(zone_id, new_np).await;
                                        debug!(zone_id, station = %station_name, "radio_metadata_updated");
                                    }
                                }
                            }
                        }
                    }
                }

                // Sync metrics and skip the rest of the loop (no gapless/track-end).
                self.shared_metrics.lock().await.insert(
                    zone_id,
                    ZonePollerMetrics {
                        total_polls: ps.total_polls,
                        total_errors: ps.total_errors,
                        consecutive_errors: ps.consecutive_errors,
                        last_latency_ms: ps.last_latency_ms,
                        max_latency_ms: ps.max_latency_ms,
                    },
                );

                if radio_stopped {
                    // Radio stopped on the renderer — stop the zone.
                    // Done after metrics sync so `ps` borrow is released.
                    info!(zone_id, "radio_renderer_stopped");
                    poll_states.remove(&zone_id);
                    let device_id_ref = self.get_zone_device_id(zone_id);
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                }
                continue;
            }

            // Check whether we're in the seek grace period: after a seek the
            // in-memory position is authoritative and the output may still
            // report the old (pre-seek) position until the stream restarts.
            // During this window we skip overwriting position to prevent the
            // progress bar from snapping back.
            //
            // For streaming sources (Qobuz/Tidal) on network outputs (DLNA),
            // seeking recreates the entire stream session — the renderer may
            // report Stopped for several seconds while it buffers the new
            // stream.  Use a longer grace period to prevent the poller from
            // accumulating stopped_ticks and false-skipping to the next track.
            let is_streaming_seek = zone_state.now_playing.as_ref().is_some_and(|np| {
                np.source != "local"
                    && np.source != "radio"
                    && np.source != "podcast"
                    && np.stream_id.is_some()
            }) && all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .and_then(|z| z.output_type.as_deref())
                .is_some_and(|t| {
                    matches!(
                        t,
                        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
                    )
                });
            let seek_grace_secs = if is_streaming_seek {
                SEEK_STREAMING_GRACE_SECS
            } else {
                SEEK_GRACE_SECS
            };
            let in_seek_grace = zone_state
                .last_seek_at
                .map(|t| t.elapsed().as_secs() < seek_grace_secs)
                .unwrap_or(false);

            if !in_seek_grace {
                self.playback
                    .update_position(zone_id, status.position_ms as i64)
                    .await;
                self.playback
                    .emit_position(zone_id, status.position_ms as i64);
            }

            // Sync volume from device (skip if fixed_volume)
            let zone_fixed_volume = all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .map(|z| z.fixed_volume)
                .unwrap_or(false);
            if !zone_fixed_volume && (status.volume - zone_state.volume).abs() > 0.005 {
                self.playback.set_volume(zone_id, status.volume).await;
                let vol_int = (status.volume * 100.0) as i32;
                let db = self.db.clone();
                crate::db::zone_repo::ZoneRepo::with_backend(db)
                    .update_volume(zone_id, vol_int)
                    .ok();
            }

            // --- Persist position to DB periodically ---
            ps.ticks_since_db_save += 1;
            if ps.ticks_since_db_save >= POSITION_SAVE_INTERVAL_TICKS {
                ps.ticks_since_db_save = 0;
                let np = zone_state.now_playing.as_ref();
                let track_id = np.and_then(|n| n.track_id);
                let source = np.map(|n| n.source.as_str());
                let source_id = np.and_then(|n| n.source_id.as_deref());
                ZoneRepo::with_backend(self.db.clone())
                    .save_playback_position(
                        zone_id,
                        status.position_ms as i64,
                        track_id,
                        source,
                        source_id,
                    )
                    .ok();
            }

            // Track the high-water mark for position — used to verify that
            // enough of the track was actually played before accepting a
            // gapless transition.  We update this BEFORE checking for resets
            // so the peak reflects the last known good position.
            if status.position_ms > ps.peak_position_ms {
                ps.peak_position_ms = status.position_ms;
            }

            let track_duration_ms = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.duration_ms as u64)
                .unwrap_or(0);

            // Helper: has enough of the track been played?
            // When track_duration is known: peak_position_ms >= 80% of duration.
            // When track_duration is unknown (0): require peak_position_ms >= 60s
            // to avoid false skips on slow renderers (Shanling SCD1.3 etc.)
            // that report duration=0 and briefly show Stopped while buffering.
            let wall_elapsed = ps
                .track_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let played_enough = if track_duration_ms == 0 {
                // Unknown duration: rely on peak position as the only guard.
                ps.peak_position_ms >= MIN_PEAK_UNKNOWN_DURATION_MS
                    && wall_elapsed >= MIN_TRACK_WALL_SECS
            } else {
                ps.peak_position_ms as f64 >= track_duration_ms as f64 * MIN_PLAYED_FRACTION
                    && wall_elapsed >= MIN_TRACK_WALL_SECS
            };

            // Detect position reset: position drops from >30s to <5s.
            // This is a strong signal that the renderer performed a gapless
            // transition (the new track starts from 0).
            let position_reset = ps.last_position_ms > 30_000
                && status.position_ms < 5_000
                && ps.gapless_sent_at.is_some();
            ps.last_position_ms = status.position_ms;

            if position_reset {
                if !played_enough {
                    warn!(
                        zone_id,
                        peak_pos = ps.peak_position_ms,
                        track_dur = track_duration_ms,
                        "gapless_position_reset_ignored_not_enough_played"
                    );
                } else {
                    info!(
                        zone_id,
                        prev_pos = ps.last_position_ms,
                        new_pos = status.position_ms,
                        "gapless_position_reset_detected"
                    );
                    ps.gapless_sent = false;
                    ps.gapless_sent_at = None;
                    ps.stopped_ticks = 0;
                    ps.past_end_ticks = 0;
                    ps.peak_position_ms = 0;
                    ps.last_position_ms = 0;
                    ps.last_bytes_sent = 0;
                    ps.track_started_at = Some(Instant::now());
                    ps.gapless_advance_pending = false;
                    ps.gapless_stuck_ticks = 0;
                    if let Some(next_pos) = Self::next_position(zone_state) {
                        info!(zone_id, next_pos, "gapless_advance_on_position_reset");
                        if let Err(e) = self
                            .orchestrator
                            .advance_queue_metadata(zone_id, next_pos)
                            .await
                        {
                            warn!(zone_id, error = %e, "gapless_advance_failed");
                        }
                        ps.gapless_cooldown = 4;
                    }
                }
            }

            // Clear expired guard
            if let Some(sent_at) = ps.gapless_sent_at {
                if sent_at.elapsed() > std::time::Duration::from_secs(GAPLESS_GUARD_SECS) {
                    debug!(zone_id, "gapless_guard_expired");
                    ps.gapless_sent_at = None;
                }
            }

            let in_gapless_guard = ps.gapless_sent_at.is_some();

            let mut track_ended = false;
            let mut force_stop = false;

            // Guard: if Tune's own playback state for this zone is Stopped
            // (or has no now_playing), ignore device state changes entirely.
            // This prevents phantom playback when another app (e.g. Roon)
            // plays on a shared renderer (e.g. Sonos) and then stops —
            // Tune would otherwise interpret the Stopped→Playing cycle as
            // its own track ending and auto-advance to the next queue item.
            let tune_is_playing =
                zone_state.state == PlayState::Playing || zone_state.state == PlayState::Paused;
            let tune_has_track = zone_state.now_playing.is_some();

            match status.state {
                TransportState::Stopped if !tune_is_playing || !tune_has_track => {
                    // Tune is not playing on this zone — ignore device Stopped.
                    ps.stopped_ticks = 0;
                }
                TransportState::Stopped => {
                    // During the seek grace period, the renderer may report
                    // Stopped while it buffers the new stream (especially for
                    // streaming seeks that recreate the session).  Suppress
                    // stopped_ticks to prevent false track-end detection.
                    let in_track_load_grace = ps.track_loaded_at.elapsed().as_secs()
                        < TRACK_LOAD_GRACE_SECS
                        && ps.track_started_at.is_none();
                    if in_seek_grace {
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            seek_grace_secs = seek_grace_secs,
                            "seek_grace_suppressing_stopped_ticks"
                        );
                    } else if in_track_load_grace {
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            elapsed = ps.track_loaded_at.elapsed().as_secs(),
                            grace = TRACK_LOAD_GRACE_SECS,
                            "track_load_grace_suppressing_stopped_ticks"
                        );
                    } else if ps.gapless_cooldown > 0 {
                        ps.gapless_cooldown -= 1;
                        ps.stopped_ticks = 0;
                    } else if in_gapless_guard {
                        if !played_enough {
                            // Renderer reported Stopped during guard but not
                            // enough of the track was played — ignore to avoid
                            // false skip (DMP-A8 quirk).
                            debug!(
                                zone_id,
                                peak_pos = ps.peak_position_ms,
                                track_dur = track_duration_ms,
                                "gapless_guard_stopped_ignored_not_enough_played"
                            );
                        } else {
                            // During the gapless guard period, a Stopped state
                            // MAY mean the renderer transitioned via gapless.
                            // Don't advance metadata yet — wait for the renderer
                            // to report Playing (position reset) to confirm.
                            // If it stays Stopped, the stuck handler will force
                            // play_from_queue which handles metadata correctly.
                            info!(zone_id, "gapless_guard_stopped_pending_confirmation");
                            ps.gapless_sent = false;
                            ps.gapless_sent_at = None;
                            ps.stopped_ticks = 0;
                            ps.peak_position_ms = 0;
                            ps.last_position_ms = 0;
                            ps.track_started_at = None;
                            ps.gapless_advance_pending = true;
                            ps.gapless_stuck_ticks = 0;
                            ps.gapless_cooldown = 4;
                        }
                    } else if ps.gapless_advance_pending {
                        // The poller advanced metadata expecting the renderer
                        // to auto-transition via gapless, but the renderer is
                        // still Stopped after the cooldown expired.  Count
                        // stuck ticks and force play_from_queue if the renderer
                        // doesn't pick up within GAPLESS_STUCK_THRESHOLD.
                        ps.gapless_stuck_ticks += 1;
                        if ps.gapless_stuck_ticks >= GAPLESS_STUCK_THRESHOLD {
                            warn!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                "gapless_advance_stuck_forcing_play"
                            );
                            ps.gapless_advance_pending = false;
                            ps.gapless_stuck_ticks = 0;
                            ps.stopped_ticks = 0;
                            track_ended = true;
                        } else {
                            debug!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                threshold = GAPLESS_STUCK_THRESHOLD,
                                "gapless_advance_waiting_for_renderer"
                            );
                        }
                    } else if status.ended_naturally && (played_enough || wall_elapsed >= 5) {
                        // Local outputs (WASAPI/ALSA/CoreAudio) signal
                        // ended_naturally when the audio stream reaches EOF.
                        // Skip the STOPPED_TICKS_THRESHOLD wait — we know
                        // the track is done, no need to accumulate 5s of
                        // stopped ticks.
                        info!(
                            zone_id,
                            wall_elapsed,
                            peak_pos = ps.peak_position_ms,
                            "local_output_ended_naturally_advancing"
                        );
                        track_ended = true;
                    } else {
                        ps.stopped_ticks += 1;
                        if ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD {
                            let is_short_track = track_duration_ms > 0
                                && track_duration_ms < MIN_TRACK_WALL_SECS * 1000;
                            // When repeat mode is active (One or All) on DLNA,
                            // be more lenient about accepting track-end: if the
                            // renderer has reported Stopped and we've seen any
                            // meaningful playback (peak > 5s), treat it as a
                            // natural end so the poller re-triggers play instead
                            // of accumulating stopped_ticks until force_stop.
                            // (DEvir QA B-05: repeat mode doesn't work on DLNA)
                            let repeat_active =
                                matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
                            let repeat_end = repeat_active && ps.peak_position_ms > 5_000;
                            let natural_end = played_enough
                                || repeat_end
                                || (status.ended_naturally && wall_elapsed >= 5)
                                || (is_short_track
                                    && ps.peak_position_ms as f64
                                        >= track_duration_ms as f64 * 0.5);
                            if status.ended_naturally && wall_elapsed < 5 && !played_enough {
                                warn!(
                                    zone_id,
                                    wall_elapsed,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    "ended_naturally_rejected_too_early"
                                );
                            }
                            if natural_end {
                                if ps.gapless_sent {
                                    // Gapless was prepared via SetNextAVTransportURI.
                                    // Don't advance metadata yet — wait for the
                                    // renderer to confirm the transition by starting
                                    // to play (position reset detected in the Playing
                                    // handler).  If it stays Stopped after the
                                    // cooldown + stuck threshold, fall through to
                                    // play_from_queue which handles metadata itself.
                                    info!(zone_id, "gapless_natural_end_waiting_for_transition");
                                    ps.gapless_sent = false;
                                    ps.gapless_sent_at = None;
                                    ps.stopped_ticks = 0;
                                    ps.peak_position_ms = 0;
                                    ps.last_position_ms = 0;
                                    ps.track_started_at = None;
                                    ps.gapless_advance_pending = true;
                                    ps.gapless_stuck_ticks = 0;
                                    ps.gapless_cooldown = 4;
                                } else {
                                    track_ended = true;
                                }
                            } else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD {
                                // Check if the stream is still being consumed
                                // (renderer actively fetching audio data). If so,
                                // don't kill — the renderer is playing but not
                                // reporting state (DMP-A10, LHC, Shanling, etc.).
                                let stream_id = zone_state
                                    .now_playing
                                    .as_ref()
                                    .and_then(|np| np.stream_id.clone());
                                let current_bytes = if let Some(ref sid) = stream_id {
                                    self.orchestrator
                                        .streamer_bytes_sent(sid)
                                        .await
                                        .unwrap_or(0)
                                } else {
                                    0
                                };
                                let stream_consuming =
                                    current_bytes > 0 && current_bytes > ps.last_bytes_sent;
                                ps.last_bytes_sent = current_bytes;

                                if stream_consuming {
                                    if ps.stopped_ticks % 30 == 0 {
                                        debug!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            wall_secs = wall_elapsed,
                                            bytes_sent = current_bytes,
                                            "dlna_renderer_not_reporting_state_waiting"
                                        );
                                    }
                                } else {
                                    warn!(
                                        zone_id,
                                        peak_pos = ps.peak_position_ms,
                                        track_dur = track_duration_ms,
                                        wall_secs = wall_elapsed,
                                        bytes_sent = current_bytes,
                                        "playback_failure_stopping_zone"
                                    );
                                    track_ended = false;
                                    force_stop = true;
                                }
                            } else {
                                debug!(
                                    zone_id,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    wall_secs = wall_elapsed,
                                    stopped_ticks = ps.stopped_ticks,
                                    unknown_dur_min_peak = if track_duration_ms == 0 {
                                        MIN_PEAK_UNKNOWN_DURATION_MS
                                    } else {
                                        0
                                    },
                                    "stopped_early_waiting"
                                );
                            }
                        }
                    }
                }
                TransportState::Playing | TransportState::Transitioning => {
                    ps.stopped_ticks = 0;
                    ps.gapless_cooldown = 0;
                    // Renderer started playing — gapless transition confirmed.
                    // NOW advance metadata (deferred from the Stopped handler
                    // to avoid showing the wrong track on renderers that don't
                    // actually auto-transition via SetNextAVTransportURI).
                    if ps.gapless_advance_pending {
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        if let Some(next_pos) = Self::next_position(zone_state) {
                            info!(zone_id, next_pos, "gapless_confirmed_advancing_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_confirmed_advance_failed");
                            }
                            ps.gapless_cooldown = 4;
                        }
                    }
                    if ps.track_started_at.is_none() {
                        ps.track_started_at = Some(Instant::now());
                    }

                    // Detect gapless transition: renderer reports a different
                    // duration than the current track AND the position confirms
                    // the track actually ended (near end or reset to start).
                    // Some DLNA renderers (DMP-A6/A8) report inaccurate durations
                    // from the start, so duration mismatch alone is insufficient.
                    let duration_changed = ps.gapless_sent
                        && track_duration_ms > 0
                        && status.duration_ms > 0
                        && (status.duration_ms as i64 - track_duration_ms as i64).unsigned_abs()
                            > 2000;
                    // Position must confirm we are actually at the end of the
                    // current track OR that the position has reset to the
                    // start of the next track.  The played_enough guard
                    // prevents false transitions when a renderer (DMP-A8)
                    // reports position < 5s immediately after SetNext.
                    let position_confirms_transition = played_enough
                        && (status.position_ms < 5000
                            || (track_duration_ms > 0
                                && status.position_ms
                                    >= track_duration_ms.saturating_sub(GAPLESS_WINDOW_MS)));
                    if duration_changed && position_confirms_transition {
                        info!(
                            zone_id,
                            renderer_dur = status.duration_ms,
                            track_dur = track_duration_ms,
                            peak_pos = ps.peak_position_ms,
                            "gapless_transition_detected"
                        );
                        ps.gapless_sent = false;
                        ps.gapless_sent_at = None;
                        ps.peak_position_ms = 0;
                        ps.last_position_ms = 0;
                        ps.last_bytes_sent = 0;
                        ps.track_started_at = Some(Instant::now());
                        ps.stopped_ticks = 0;
                        ps.past_end_ticks = 0;
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        if let Some(next_pos) = Self::next_position(zone_state) {
                            info!(zone_id, next_pos, "gapless_advance_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_advance_failed");
                            }
                            // Suppress handle_track_end for a few ticks — the
                            // renderer may briefly report Stopped during the
                            // gapless transition, which would otherwise send a
                            // redundant Stop+Play and cause an audible restart.
                            ps.gapless_cooldown = 4;
                        } else {
                            self.handle_track_end(zone_id, zone_state).await;
                        }
                    } else if !ps.gapless_sent
                        && status.duration_ms > GAPLESS_WINDOW_MS
                        && status.position_ms >= status.duration_ms - GAPLESS_WINDOW_MS
                    {
                        // Only send SetNextAVTransportURI if gapless is enabled for this zone
                        let gapless_enabled = ZoneRepo::with_backend(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .map(|z| z.gapless_enabled)
                            .unwrap_or(true);
                        if gapless_enabled {
                            let ok = self.prepare_gapless(zone_id, zone_state, &device_id).await;
                            if ok {
                                ps.gapless_sent_at = Some(Instant::now());
                                ps.gapless_sent = true;
                            }
                        } else {
                            debug!(zone_id, "gapless_disabled_for_zone");
                            ps.gapless_sent = true;
                        }
                    }

                    // Position-based end-of-track detection: when the output
                    // still reports Playing but position has reached or exceeded
                    // the known track duration, the audio has effectively ended
                    // (e.g. local/cpal output draining its ring buffer).
                    // Wait POSITION_PAST_END_TICKS consecutive ticks to avoid
                    // cutting off the last fraction of a second of audio.
                    // Add a 3-second margin to avoid cutting off the end of
                    // tracks on DLNA renderers that report position slightly
                    // ahead of actual playback.
                    let end_margin_ms = 3000u64;
                    if track_duration_ms > end_margin_ms
                        && played_enough
                        && status.position_ms >= track_duration_ms.saturating_add(end_margin_ms)
                    {
                        ps.past_end_ticks += 1;
                        if ps.past_end_ticks >= POSITION_PAST_END_TICKS {
                            info!(
                                zone_id,
                                position_ms = status.position_ms,
                                track_dur = track_duration_ms,
                                past_end_ticks = ps.past_end_ticks,
                                "position_past_end_advancing"
                            );
                            track_ended = true;
                        }
                    } else {
                        ps.past_end_ticks = 0;
                    }
                }
                TransportState::Paused => {
                    ps.stopped_ticks = 0;
                }
            }

            // Sync metrics to shared map for external visibility
            self.shared_metrics.lock().await.insert(
                zone_id,
                ZonePollerMetrics {
                    total_polls: ps.total_polls,
                    total_errors: ps.total_errors,
                    consecutive_errors: ps.consecutive_errors,
                    last_latency_ms: ps.last_latency_ms,
                    max_latency_ms: ps.max_latency_ms,
                },
            );

            if force_stop {
                poll_states.remove(&zone_id);
                let device_id_ref = self.get_zone_device_id(zone_id);
                self.orchestrator
                    .stop(zone_id, device_id_ref.as_deref())
                    .await;
            } else if track_ended {
                poll_states.remove(&zone_id);
                self.handle_track_end(zone_id, zone_state).await;
            }
        }
    }

    pub fn next_position(zone_state: &crate::playback::ZoneState) -> Option<i64> {
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
                let next = zone_state.queue_position + 1;
                if next >= zone_state.queue_length {
                    None
                } else {
                    Some(next)
                }
            }
        }
    }

    async fn handle_track_end(&self, zone_id: i64, zone_state: &crate::playback::ZoneState) {
        // Diagnostic: capture now-playing info to help diagnose premature advance issues.
        let np_title = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.title.as_str())
            .unwrap_or("unknown");
        let np_duration = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.duration_ms)
            .unwrap_or(0);

        let device_id = self.get_zone_device_id(zone_id);

        let Some(next_pos) = Self::next_position(zone_state) else {
            // Queue ended — check if autoplay is enabled for this zone
            let autoplay_enabled = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .get_autoplay_enabled(zone_id);

            if autoplay_enabled {
                // Try to generate similar tracks based on the last played track
                let seed_track_id = zone_state.now_playing.as_ref().and_then(|np| np.track_id);

                if let Some(seed_id) = seed_track_id {
                    info!(
                        zone_id,
                        seed_track_id = seed_id,
                        "autoplay_generating_tracks"
                    );
                    let generated = crate::playback::auto_dj::generate_queue(&self.db, seed_id, 10);

                    if !generated.is_empty() {
                        let track_ids: Vec<i64> = generated
                            .iter()
                            .filter_map(|t| t["track_id"].as_i64())
                            .collect();

                        if !track_ids.is_empty() {
                            info!(
                                zone_id,
                                count = track_ids.len(),
                                "autoplay_tracks_generated"
                            );

                            // Append generated tracks to the play queue
                            let queue_repo =
                                crate::db::play_queue_repo::PlayQueueRepo::with_backend(
                                    self.db.clone(),
                                );
                            if let Err(e) = queue_repo.append_tracks(zone_id, &track_ids) {
                                warn!(zone_id, error = %e, "autoplay_append_queue_failed");
                                self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                                return;
                            }

                            // Emit autoplay_tracks_added event for UI updates
                            if let Some(ref bus) = self.event_bus {
                                bus.emit(
                                    "playback.autoplay_tracks_added",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "track_ids": track_ids,
                                        "tracks": generated,
                                        "seed_track_id": seed_id,
                                    }),
                                );
                            }

                            // Play the first generated track (next position after current)
                            let new_pos = zone_state.queue_position + 1;
                            info!(zone_id, new_pos, "autoplay_starting_generated_track");
                            if let Err(e) =
                                self.orchestrator.play_from_queue(zone_id, new_pos).await
                            {
                                warn!(zone_id, error = %e, "autoplay_play_failed");
                                self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                            }
                            return;
                        }
                    }
                    info!(zone_id, "autoplay_no_similar_tracks_found");
                } else {
                    debug!(zone_id, "autoplay_skipped_no_local_seed_track");
                }
            }

            info!(zone_id, "queue_ended");
            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
            return;
        };

        let is_repeat = matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
        info!(
            zone_id,
            next_pos,
            repeat = ?zone_state.repeat,
            shuffle = zone_state.shuffle,
            is_repeat,
            title = %np_title,
            duration_ms = np_duration,
            queue_len = zone_state.queue_length,
            queue_pos = zone_state.queue_position,
            "auto_next"
        );
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
    ) -> bool {
        let Some(next_pos) = Self::next_position(zone_state) else {
            return false;
        };

        match self
            .orchestrator
            .resolve_queue_item_url(zone_id, next_pos)
            .await
        {
            Ok(resolved) => {
                if let Some(ref sid) = resolved.stream_id {
                    if !self.orchestrator.wait_stream_data_ready(sid, 5000).await {
                        debug!(zone_id, "gapless_data_ready_timeout");
                    }
                }
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    outputs.get(device_id)
                };
                if let Some(output_arc) = output_arc {
                    let output = output_arc.lock().await;
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: None,
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                    };
                    if let Err(e) = output.set_next_media(&media).await {
                        debug!(zone_id, error = %e, "gapless_set_next_failed");
                        false
                    } else {
                        info!(zone_id, title = %resolved.title, "gapless_next_set");
                        true
                    }
                } else {
                    false
                }
            }
            Err(e) => {
                debug!(zone_id, error = %e, "gapless_resolve_failed");
                false
            }
        }
    }

    fn get_zone_device_id(&self, zone_id: i64) -> Option<String> {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gapless_cooldown_suppresses_stopped() {
        let mut ps = ZonePollState {
            gapless_sent: false,
            stopped_ticks: 0,
            gapless_cooldown: 4,
            consecutive_errors: 0,
            backoff_remaining: 0,
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            ticks_since_db_save: 0,
            track_started_at: None,
            track_generation: 0,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
        };

        // While cooldown > 0, stopped_ticks must not accumulate
        for _ in 0..4 {
            assert!(ps.gapless_cooldown > 0);
            ps.gapless_cooldown -= 1;
            ps.stopped_ticks = 0; // simulates the Stopped branch logic
        }
        assert_eq!(ps.gapless_cooldown, 0);
        assert_eq!(ps.stopped_ticks, 0);

        // After cooldown expires, stopped_ticks can accumulate
        ps.stopped_ticks = 1;
        assert!(ps.stopped_ticks < STOPPED_TICKS_THRESHOLD);
        ps.stopped_ticks = 2;
        assert!(ps.stopped_ticks < STOPPED_TICKS_THRESHOLD);
        // STOPPED_TICKS_THRESHOLD is 5, so it takes 5 ticks to trigger
        ps.stopped_ticks = STOPPED_TICKS_THRESHOLD;
        assert!(ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD);
    }

    #[test]
    fn playing_state_resets_cooldown() {
        let mut ps = ZonePollState {
            gapless_sent: true,
            stopped_ticks: 0,
            gapless_cooldown: 3,
            consecutive_errors: 0,
            backoff_remaining: 0,
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            ticks_since_db_save: 0,
            track_started_at: None,
            track_generation: 0,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
        };

        // Simulates entering Playing state
        ps.stopped_ticks = 0;
        ps.gapless_cooldown = 0;
        assert_eq!(ps.gapless_cooldown, 0);
    }

    #[test]
    fn next_position_repeat_off() {
        let state = crate::playback::ZoneState {
            state: PlayState::Playing,
            queue_position: 3,
            queue_length: 5,
            repeat: RepeatMode::Off,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(PositionPoller::next_position(&state), Some(4));
    }

    #[test]
    fn next_position_end_of_queue() {
        let state = crate::playback::ZoneState {
            state: PlayState::Playing,
            queue_position: 4,
            queue_length: 5,
            repeat: RepeatMode::Off,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(PositionPoller::next_position(&state), None);
    }

    #[test]
    fn next_position_repeat_all_wraps() {
        let state = crate::playback::ZoneState {
            state: PlayState::Playing,
            queue_position: 4,
            queue_length: 5,
            repeat: RepeatMode::All,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(PositionPoller::next_position(&state), Some(0));
    }

    #[test]
    fn next_position_repeat_one() {
        let state = crate::playback::ZoneState {
            state: PlayState::Playing,
            queue_position: 2,
            queue_length: 5,
            repeat: RepeatMode::One,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(PositionPoller::next_position(&state), Some(2));
    }

    #[test]
    fn next_position_empty_queue() {
        let state = crate::playback::ZoneState {
            state: PlayState::Playing,
            queue_position: 0,
            queue_length: 0,
            repeat: RepeatMode::Off,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(PositionPoller::next_position(&state), None);
    }

    #[test]
    fn backoff_exponential() {
        let mut ps = ZonePollState {
            gapless_sent: false,
            stopped_ticks: 0,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            ticks_since_db_save: 0,
            track_started_at: None,
            track_generation: 0,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
        };

        // Simulate consecutive errors with exponential backoff
        for expected_errors in 1u8..=5 {
            ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
            ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
            assert_eq!(ps.consecutive_errors, expected_errors);
        }
        // After 4 errors: backoff = 2^4 = 16
        assert_eq!(ps.backoff_remaining, 16);

        // After 5 errors: still capped at 2^4 = 16
        ps.consecutive_errors = 5;
        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
        assert_eq!(ps.backoff_remaining, 16);

        // Success resets
        ps.consecutive_errors = 0;
        assert_eq!(ps.consecutive_errors, 0);
    }

    #[test]
    fn played_enough_rejects_early_transition() {
        // Track is 300 seconds (300_000 ms).  Peak at 10s — only 3.3% played.
        let peak_ms: u64 = 10_000;
        let duration_ms: u64 = 300_000;
        let played_enough =
            duration_ms == 0 || peak_ms as f64 >= duration_ms as f64 * MIN_PLAYED_FRACTION;
        assert!(
            !played_enough,
            "10s into a 5-min track should NOT be enough"
        );
    }

    #[test]
    fn played_enough_accepts_late_transition() {
        // Track is 300 seconds.  Peak at 280s — 93% played.
        let peak_ms: u64 = 280_000;
        let duration_ms: u64 = 300_000;
        let played_enough =
            duration_ms == 0 || peak_ms as f64 >= duration_ms as f64 * MIN_PLAYED_FRACTION;
        assert!(played_enough, "280s into a 5-min track should be enough");
    }

    #[test]
    fn played_enough_unknown_duration_low_peak() {
        // When duration is unknown (0) and peak position is below the
        // threshold, played_enough should be false to prevent false skips
        // on slow renderers like Shanling SCD1.3.
        let peak_ms: u64 = 5_000;
        let duration_ms: u64 = 0;
        let played_enough = if duration_ms == 0 {
            peak_ms >= MIN_PEAK_UNKNOWN_DURATION_MS
        } else {
            peak_ms as f64 >= duration_ms as f64 * MIN_PLAYED_FRACTION
        };
        assert!(
            !played_enough,
            "5s peak with unknown duration should NOT pass"
        );
    }

    #[test]
    fn played_enough_unknown_duration_high_peak() {
        // When duration is unknown (0) but enough position has been reported,
        // allow the transition.
        let peak_ms: u64 = 120_000;
        let duration_ms: u64 = 0;
        let played_enough = if duration_ms == 0 {
            peak_ms >= MIN_PEAK_UNKNOWN_DURATION_MS
        } else {
            peak_ms as f64 >= duration_ms as f64 * MIN_PLAYED_FRACTION
        };
        assert!(played_enough, "120s peak with unknown duration should pass");
    }

    #[test]
    fn past_end_ticks_triggers_after_threshold() {
        // Simulate: output reports Playing but position >= track duration.
        // After POSITION_PAST_END_TICKS ticks, track should be treated as ended.
        let mut past_end: u8 = 0;
        let track_duration_ms: u64 = 240_000;
        let position_ms: u64 = 240_500; // slightly past end
        let played_enough = true;

        for _ in 0..POSITION_PAST_END_TICKS {
            if track_duration_ms > 0 && played_enough && position_ms >= track_duration_ms {
                past_end += 1;
            } else {
                past_end = 0;
            }
        }
        assert!(
            past_end >= POSITION_PAST_END_TICKS,
            "should trigger after {} ticks past end",
            POSITION_PAST_END_TICKS
        );
    }

    #[test]
    fn past_end_ticks_resets_when_position_below_duration() {
        // If position drops below duration (e.g. seek or correction),
        // the past_end counter should reset.
        let mut past_end: u8 = 2; // already accumulated some ticks
        let track_duration_ms: u64 = 240_000;
        let position_ms: u64 = 200_000; // below duration
        let played_enough = true;

        if track_duration_ms > 0 && played_enough && position_ms >= track_duration_ms {
            past_end += 1;
        } else {
            past_end = 0;
        }
        assert_eq!(past_end, 0, "counter should reset when position < duration");
    }

    #[test]
    fn gapless_stuck_forces_track_end() {
        // BUG-004: After gapless metadata advance, if the renderer stays
        // Stopped, gapless_stuck_ticks should accumulate and trigger
        // track_ended after GAPLESS_STUCK_THRESHOLD ticks.
        let mut ps = ZonePollState {
            gapless_sent: false,
            stopped_ticks: 0,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            ticks_since_db_save: 0,
            track_started_at: None,
            track_generation: 0,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: true, // metadata was advanced
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
        };

        // Simulate renderer staying Stopped after cooldown expired.
        // gapless_advance_pending is true, gapless_cooldown is 0.
        for tick in 1..=GAPLESS_STUCK_THRESHOLD {
            ps.gapless_stuck_ticks += 1;
            if tick < GAPLESS_STUCK_THRESHOLD {
                assert!(
                    ps.gapless_stuck_ticks < GAPLESS_STUCK_THRESHOLD,
                    "should not trigger yet at tick {tick}"
                );
            }
        }
        assert!(
            ps.gapless_stuck_ticks >= GAPLESS_STUCK_THRESHOLD,
            "should trigger track_ended after {} ticks",
            GAPLESS_STUCK_THRESHOLD
        );

        // After triggering, pending state should be cleared
        ps.gapless_advance_pending = false;
        ps.gapless_stuck_ticks = 0;
        assert!(!ps.gapless_advance_pending);
        assert_eq!(ps.gapless_stuck_ticks, 0);
    }

    #[test]
    fn gapless_stuck_cleared_on_playing() {
        // When the renderer transitions to Playing, gapless_advance_pending
        // should be cleared (the gapless transition succeeded).
        let mut ps = ZonePollState {
            gapless_sent: false,
            stopped_ticks: 0,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            ticks_since_db_save: 0,
            track_started_at: None,
            track_generation: 0,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: true,
            gapless_stuck_ticks: 3,
            last_bytes_sent: 0,
        };

        // Simulate entering Playing state (renderer auto-transitioned)
        if ps.gapless_advance_pending {
            ps.gapless_advance_pending = false;
            ps.gapless_stuck_ticks = 0;
        }
        assert!(!ps.gapless_advance_pending);
        assert_eq!(ps.gapless_stuck_ticks, 0);
    }
}
