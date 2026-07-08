//! Native SlimProto output — plays to a Squeezelite/slim2diretta player that is
//! connected to Tune's own SlimProto server (`crate::slimproto`, port 3483).
//!
//! Unlike [`crate::outputs::squeezebox::SqueezeboxOutput`] (which drives a player
//! through an *external* LMS via its CLI), this output speaks the SlimProto wire
//! protocol directly: playback commands are sent to the connected player through
//! a per-player command channel owned by the server, and status is read back from
//! the shared player registry (updated by the player's STAT messages).
//!
//! Phase 1 (this commit) wires visibility only: the zone appears/goes online and
//! `get_status()` reports the live position from STAT. The playback commands are
//! filled in Phase 2 (the `strm` start/pause/stop path).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::slimproto::PlayerRegistry;

/// Transport state stored as a `u8` so it can live behind an `Arc<AtomicU8>`.
const ST_STOPPED: u8 = 0;
const ST_PLAYING: u8 = 1;
const ST_PAUSED: u8 = 2;

/// A native SlimProto audio output bound to one connected player (by MAC).
pub struct SlimProtoOutput {
    name: String,
    device_id: String,
    /// MAC string key into the shared player registry.
    mac_str: String,
    /// Shared registry of connected players (position/state read from here).
    players: PlayerRegistry,
    /// Locally-tracked transport state (set by the playback commands).
    state: Arc<AtomicU8>,
}

impl SlimProtoOutput {
    pub fn new(name: String, device_id: String, mac_str: String, players: PlayerRegistry) -> Self {
        Self {
            name,
            device_id,
            mac_str,
            players,
            state: Arc::new(AtomicU8::new(ST_STOPPED)),
        }
    }

    fn transport(&self) -> TransportState {
        match self.state.load(Ordering::Relaxed) {
            ST_PLAYING => TransportState::Playing,
            ST_PAUSED => TransportState::Paused,
            _ => TransportState::Stopped,
        }
    }
}

#[async_trait]
impl OutputTarget for SlimProtoOutput {
    fn name(&self) -> &str {
        &self.name
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        "slimproto"
    }

    // --- Phase 2 fills these in (strm start/pause/unpause/stop). For now they
    // --- are no-ops so the zone is selectable without erroring. ---
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        self.state.store(ST_STOPPED, Ordering::Relaxed);
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let position_ms = {
            let reg = self.players.lock().await;
            reg.get(&self.mac_str).map(|p| p.elapsed_ms as u64)
        };
        Ok(OutputStatus {
            state: self.transport(),
            position_ms: position_ms.unwrap_or(0),
            duration_ms: 0,
            volume: 1.0,
            muted: false,
            current_uri: None,
            track_title: None,
            track_artist: None,
            ended_naturally: false,
        })
    }

    async fn is_available(&self) -> bool {
        let reg = self.players.lock().await;
        reg.contains_key(&self.mac_str)
    }
}
