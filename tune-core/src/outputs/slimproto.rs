//! Native SlimProto output — plays to a Squeezelite/slim2diretta player that is
//! connected to Tune's own SlimProto server (`crate::slimproto`, port 3483).
//!
//! Unlike [`crate::outputs::squeezebox::SqueezeboxOutput`] (which drives a player
//! through an *external* LMS via its CLI), this output speaks the SlimProto wire
//! protocol directly: playback commands (`strm`/`audg`) are pushed to the
//! connected player through a per-player command channel owned by the server, and
//! status is read back from the shared player registry (updated by STAT).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::slimproto::{
    CommandChannels, PlayerRegistry, ServerMessage, build_strm_start, strm_control,
};

/// Transport state stored as a `u8` so it can live behind an `Arc<AtomicU8>`.
const ST_STOPPED: u8 = 0;
const ST_PLAYING: u8 = 1;
const ST_PAUSED: u8 = 2;

/// A native SlimProto audio output bound to one connected player (by MAC).
pub struct SlimProtoOutput {
    name: String,
    device_id: String,
    /// MAC string key into the shared player registry and command channels.
    mac_str: String,
    /// Shared registry of connected players (position/state read from here).
    players: PlayerRegistry,
    /// Per-player command senders owned by the server (push `strm`/`audg`).
    command_channels: CommandChannels,
    /// Locally-tracked transport state (set by the playback commands).
    state: Arc<AtomicU8>,
    /// Last volume command accepted by the player's command channel.
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
}

impl SlimProtoOutput {
    pub fn new(
        name: String,
        device_id: String,
        mac_str: String,
        players: PlayerRegistry,
        command_channels: CommandChannels,
    ) -> Self {
        Self {
            name,
            device_id,
            mac_str,
            players,
            command_channels,
            state: Arc::new(AtomicU8::new(ST_STOPPED)),
            volume: Arc::new(Mutex::new(1.0)),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    fn transport(&self) -> TransportState {
        match self.state.load(Ordering::Relaxed) {
            ST_PLAYING => TransportState::Playing,
            ST_PAUSED => TransportState::Paused,
            _ => TransportState::Stopped,
        }
    }

    /// Push a message to this player's writer task via its command channel.
    async fn send(&self, msg: ServerMessage) -> Result<(), String> {
        let tx = {
            let chans = self.command_channels.lock().await;
            chans.get(&self.mac_str).cloned()
        };
        match tx {
            Some(tx) => tx
                .send(msg)
                .await
                .map_err(|_| "slimproto player command channel closed".to_string()),
            None => Err("slimproto player not connected".to_string()),
        }
    }
}

/// Split an `http://host:port/path` stream URL into `(port, path)`. `server_ip=0`
/// in the `strm s` frame makes the player reuse its control-connection server IP,
/// so only the HTTP port and request path are needed.
fn parse_stream_url(url: &str) -> Option<(u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    let path = rest[slash..].to_string();
    let port = authority
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    Some((port, path))
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

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, false, true, true, false)
    }

    /// Native SlimProto has no internal next-track staging yet (phase 3 wires
    /// `set_next_url` → `strm s` autostart). Rely on the poller's natural-end
    /// advance for now so a single-track Repeat queue still loops.
    fn supports_internal_gapless(&self) -> bool {
        false
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let (port, path) = parse_stream_url(media.url)
            .ok_or_else(|| format!("slimproto: unparseable stream url {}", media.url))?;
        self.send(build_strm_start(port, &path)).await?;
        self.state.store(ST_PLAYING, Ordering::Relaxed);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.send(strm_control(b'p')).await?;
        self.state.store(ST_PAUSED, Ordering::Relaxed);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.send(strm_control(b'u')).await?;
        self.state.store(ST_PLAYING, Ordering::Relaxed);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        // Best-effort: the player may already be gone; ignore a closed channel.
        let _ = self.send(strm_control(b'q')).await;
        self.state.store(ST_STOPPED, Ordering::Relaxed);
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek not supported on SlimProto".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // SlimProto digital gain is fixed-point with 65536 = unity.
        let volume = volume.clamp(0.0, 1.0);
        let g = (volume * 65536.0).round() as u32;
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        *self.volume.lock().await = volume;
        self.muted.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let volume = *self.volume.lock().await;
        let g = if muted {
            0
        } else {
            (volume * 65536.0).round() as u32
        };
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        self.muted.store(muted, Ordering::Relaxed);
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
            volume: *self.volume.lock().await,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri: None,
            track_title: None,
            track_artist: None,
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        let reg = self.players.lock().await;
        reg.contains_key(&self.mac_str)
    }

    /// Best-effort diagnostics for remote debugging of a tester's player
    /// (Sandro): last STAT event, position, bytes and staleness. Uses a
    /// non-blocking `try_lock` so it never stalls a status call.
    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        let reg = self.players.try_lock().ok()?;
        let p = reg.get(&self.mac_str)?;
        Some(serde_json::json!({
            "mac": self.mac_str,
            "transport": match self.transport() {
                TransportState::Playing => "playing",
                TransportState::Paused => "paused",
                TransportState::Transitioning => "transitioning",
                TransportState::Stopped => "stopped",
            },
            "elapsed_ms": p.elapsed_ms,
            "bytes_received": p.bytes_received,
            "last_stat_event": String::from_utf8_lossy(&p.last_event).trim_end_matches('\0'),
            "last_stat_secs_ago": p.last_stat.elapsed().as_secs(),
        }))
    }
}
