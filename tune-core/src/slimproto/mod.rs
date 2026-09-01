//! SlimProto TCP server — accepts direct connections from Squeezelite players
//! without needing LMS (Logitech Media Server).
//!
//! The protocol is binary, big-endian. Messages flow in both directions:
//!
//! **Client → Server** (prefixed with 4-byte tag + data):
//!   `HELO`, `STAT`, `RESP`, `META`, `DSCO`, `BYE!`
//!
//! **Server → Client** (2-byte length + 4-byte tag + payload):
//!   `strm`, `audg`, `setd`, `serv`

pub mod cli_server;
pub mod discovery;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::outputs::TransportState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_PORT: u16 = 3483;

/// Sanity cap on a client message payload length (SqueezeBox control messages
/// are tiny — HELO ~172 bytes). Rejects a mis-framed/huge length before we try
/// to allocate for it.
const MAX_MESSAGE_LEN: usize = 1024 * 1024;
const HEARTBEAT_INTERVAL_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// SlimProto message types
// ---------------------------------------------------------------------------

/// A message received from a Squeezelite client.
#[derive(Debug)]
pub enum ClientMessage {
    /// `HELO` — handshake.
    Helo {
        device_type: u8,
        firmware_version: u8,
        mac: [u8; 6],
        /// Display label derived from the modern HELO capabilities, or from
        /// the legacy trailing UTF-8 field for old short payloads.
        name: String,
    },
    /// `STAT` — status report.
    Stat {
        /// 4-byte ASCII event code, e.g. `STMd`, `STMc`, `STMt`.
        event: [u8; 4],
        /// Number of bytes received by the player.
        bytes_received: u64,
        /// Signal strength (0-100, Wi-Fi quality).
        signal_strength: u16,
        /// Elapsed milliseconds into the current track.
        elapsed_ms: u32,
        /// Raw payload for future extension.
        raw: Vec<u8>,
    },
    /// `RESP` — HTTP response headers forwarded by the player.
    Resp { data: Vec<u8> },
    /// `META` — stream metadata.
    Meta { data: Vec<u8> },
    /// `DSCO` — player disconnected from the audio stream.
    Dsco { reason: u8 },
    /// `BYE!` — player is shutting down.
    Bye,
    /// Unknown/unrecognized command tag.
    Unknown { tag: [u8; 4], data: Vec<u8> },
}

/// A message sent from the server to a Squeezelite client.
#[derive(Debug)]
pub enum ServerMessage {
    /// `strm` — stream control.
    Strm {
        /// `s` = start, `p` = pause, `u` = unpause, `q` = stop, `t` = status query.
        command: u8,
        /// Additional payload bytes (command-dependent).
        payload: Vec<u8>,
    },
    /// `audg` — volume/gain control.
    Audg {
        left_gain: u32,
        right_gain: u32,
        /// 1 = digital volume adjust, 0 = analog.
        digital_volume: u8,
    },
    /// `setd` — set device display (for players with screens).
    Setd { data: Vec<u8> },
    /// `serv` — server info.
    Serv { data: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Read one client→server message from the TCP stream.
///
/// Wire format (client → server): `[4 bytes: tag] [4 bytes: length BE] [N bytes:
/// payload]` where length = N (payload only). This is the standard
/// SlimProto/SqueezeBox client framing — e.g. a `HELO` from slim2diretta:
/// `48 45 4c 4f | 00 00 00 ac | …` (Progman). The previous code read
/// `[2-byte length][4-byte tag]` (the *server → client* framing), which
/// misparsed every client message and hung the handshake.
pub async fn read_message(stream: &mut TcpStream) -> Result<ClientMessage, String> {
    // 1. Read the 4-byte command tag.
    let mut tag = [0u8; 4];
    stream
        .read_exact(&mut tag)
        .await
        .map_err(|e| format!("read tag: {e}"))?;

    // 2. Read the 4-byte big-endian payload length.
    let payload_len = stream
        .read_u32()
        .await
        .map_err(|e| format!("read length: {e}"))? as usize;

    // Guard against an absurd allocation from a mis-framed / hostile client.
    if payload_len > MAX_MESSAGE_LEN {
        return Err(format!("payload too large: {payload_len} bytes"));
    }

    // 3. Read the payload.
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("read payload ({payload_len} bytes): {e}"))?;
    }

    debug!(
        tag = %String::from_utf8_lossy(&tag),
        payload_len,
        "slimproto_msg_received"
    );

    parse_client_message(tag, payload)
}

const MODERN_HELO_BASE_LEN: usize = 36;
const LEGACY_HELO_NAME_OFFSET: usize = 10;

fn helo_display_name(payload: &[u8], device_type: u8) -> String {
    if payload.len() >= MODERN_HELO_BASE_LEN {
        // Modern HELO layout (payload offsets): device/revision/MAC [0..8],
        // UUID [8..24], WLAN channels [24..26], bytes received [26..34],
        // language [34..36], then comma-separated ASCII capabilities.
        let capabilities = String::from_utf8_lossy(&payload[MODERN_HELO_BASE_LEN..]);
        let capabilities = capabilities.trim_matches('\0').trim();
        let capability = |key: &str| {
            capabilities
                .split(',')
                .find_map(|item| item.trim().strip_prefix(key))
                .filter(|value| !value.is_empty())
        };
        return capability("ModelName=")
            .or_else(|| capability("Model="))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Squeezebox {device_type}"));
    }

    // Preserve the old parser for short legacy frames whose optional trailing
    // field starts after device/revision/MAC/WLAN. Their layout predates the
    // 36-byte UUID/counters/language contract above.
    payload
        .get(LEGACY_HELO_NAME_OFFSET..)
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .trim_end_matches('\0')
                .to_string()
        })
        .unwrap_or_default()
}

/// Parse raw tag + payload into a typed `ClientMessage`.
fn parse_client_message(tag: [u8; 4], payload: Vec<u8>) -> Result<ClientMessage, String> {
    match &tag {
        b"HELO" => {
            let device_type = *payload.first().unwrap_or(&0);
            let firmware_version = *payload.get(1).unwrap_or(&0);
            let mut mac = [0u8; 6];
            if payload.len() >= 8 {
                mac.copy_from_slice(&payload[2..8]);
            }
            let name = helo_display_name(&payload, device_type);

            Ok(ClientMessage::Helo {
                device_type,
                firmware_version,
                mac,
                name,
            })
        }
        b"STAT" => {
            let mut event = [0u8; 4];
            if payload.len() >= 4 {
                event.copy_from_slice(&payload[..4]);
            }

            // Parse the fixed-size fields that follow the event code.
            // Layout after event[4]:
            //   [1] num_crlf
            //   [1] mas_initialized ('m')
            //   [1] mas_mode
            //   [4] buffer_size (u32 BE)
            //   [4] fullness (u32 BE)
            //   [8] bytes_received (u64 BE)
            //   [2] signal_strength (u16 BE)
            //   [4] jiffies (u32 BE)
            //   [4] output_buffer_size (u32 BE)
            //   [4] output_buffer_fullness (u32 BE)
            //   [4] elapsed_seconds (u32 BE)
            //   [2] voltage (u16 BE)
            //   [4] elapsed_ms (u32 BE)
            //   [4] server_timestamp (u32 BE)
            //   [2] error_code (u16 BE)

            let bytes_received = if payload.len() >= 19 {
                u64::from_be_bytes([
                    payload[7],
                    payload[8],
                    payload[9],
                    payload[10],
                    payload[11],
                    payload[12],
                    payload[13],
                    payload[14],
                ])
            } else {
                0
            };

            let signal_strength = if payload.len() >= 21 {
                u16::from_be_bytes([payload[15], payload[16]])
            } else {
                0
            };

            let elapsed_ms = if payload.len() >= 39 {
                let be = u32::from_be_bytes([payload[35], payload[36], payload[37], payload[38]]);
                // Some Windows Squeezelite builds send elapsed in LE.
                // Heuristic: if BE value is absurd (>24h), try LE.
                if be > 86_400_000 {
                    u32::from_le_bytes([payload[35], payload[36], payload[37], payload[38]])
                } else {
                    be
                }
            } else {
                0
            };

            Ok(ClientMessage::Stat {
                event,
                bytes_received,
                signal_strength,
                elapsed_ms,
                raw: payload,
            })
        }
        b"RESP" => Ok(ClientMessage::Resp { data: payload }),
        b"META" => Ok(ClientMessage::Meta { data: payload }),
        b"DSCO" => {
            let reason = *payload.first().unwrap_or(&0);
            Ok(ClientMessage::Dsco { reason })
        }
        b"BYE!" => Ok(ClientMessage::Bye),
        _ => Ok(ClientMessage::Unknown { tag, data: payload }),
    }
}

/// Write one server→client message to the TCP stream.
///
/// Wire format: `[2 bytes: total remaining length BE] [4 bytes: tag] [payload]`
pub async fn write_message(stream: &mut TcpStream, msg: &ServerMessage) -> Result<(), String> {
    let (tag, payload) = match msg {
        ServerMessage::Strm { command, payload } => {
            // The `strm` command byte is prepended to the extra payload.
            let mut buf = Vec::with_capacity(1 + payload.len());
            buf.push(*command);
            buf.extend_from_slice(payload);
            (*b"strm", buf)
        }
        ServerMessage::Audg {
            left_gain,
            right_gain,
            digital_volume,
        } => {
            // audg payload: [4] old_left_gain, [4] old_right_gain,
            //               [1] digital_volume_control,
            //               [1] preamp,
            //               [4] new_left_gain, [4] new_right_gain
            let mut buf = Vec::with_capacity(18);
            // Old gains (deprecated but must be present)
            buf.extend_from_slice(&left_gain.to_be_bytes());
            buf.extend_from_slice(&right_gain.to_be_bytes());
            // Digital volume flag + preamp (0)
            buf.push(*digital_volume);
            buf.push(0); // preamp
            // New gains
            buf.extend_from_slice(&left_gain.to_be_bytes());
            buf.extend_from_slice(&right_gain.to_be_bytes());
            (*b"audg", buf)
        }
        ServerMessage::Setd { data } => (*b"setd", data.clone()),
        ServerMessage::Serv { data } => (*b"serv", data.clone()),
    };

    let total_len = (4 + payload.len()) as u16;

    debug!(
        tag = %String::from_utf8_lossy(&tag),
        payload_len = payload.len(),
        "slimproto_msg_sent"
    );

    stream
        .write_u16(total_len)
        .await
        .map_err(|e| format!("write length: {e}"))?;
    stream
        .write_all(&tag)
        .await
        .map_err(|e| format!("write tag: {e}"))?;
    if !payload.is_empty() {
        stream
            .write_all(&payload)
            .await
            .map_err(|e| format!("write payload: {e}"))?;
    }
    stream.flush().await.map_err(|e| format!("flush: {e}"))?;

    Ok(())
}

/// Build a `strm t` (status query / heartbeat) message.
fn strm_status_query() -> ServerMessage {
    // `strm` with command byte 't' and no extra payload.
    // The full strm command has a fixed header of fields that the player
    // expects. We send zeroes for all optional fields.
    //
    // strm format after the 't' command byte:
    //   [1] autostart ('0'=no, '1'=auto)
    //   [1] format ('m'=mp3, 'f'=flac, 'p'=pcm, etc.)
    //   [1] pcm_sample_size
    //   [1] pcm_sample_rate
    //   [1] pcm_channels
    //   [1] pcm_endian
    //   [1] threshold (KB)
    //   [1] spdif_enable
    //   [1] transition_period
    //   [1] transition_type
    //   [1] flags
    //   [1] output_threshold
    //   [1] slaves_flag
    //   [4] replay_gain (u32 BE)
    //   [2] server_port (u16 BE)
    //   [4] server_ip (u32 BE)
    //   ... followed by optional HTTP request string
    //
    // For a status query ('t'), all fields after the command byte are ignored
    // by the player, so we send zeroes.
    let zeros = vec![0u8; 23]; // 23 bytes of fixed fields after command byte
    ServerMessage::Strm {
        command: b't',
        payload: zeros,
    }
}

/// Build a `strm s` (start-stream) message telling the player to fetch and play
/// an HTTP stream from this server. `server_ip = 0` tells the player to reuse the
/// server IP of its control connection (i.e. Tune), so we only need the HTTP port
/// and the request path. FLAC is self-describing, so the PCM fields are `'?'`.
pub fn build_strm_start(server_port: u16, http_path: &str) -> ServerMessage {
    let mut p = Vec::with_capacity(23 + http_path.len() + 20);
    p.push(b'1'); // autostart: play as soon as buffered
    p.push(b'f'); // format: FLAC
    p.push(b'?'); // pcm_sample_size (self-describing)
    p.push(b'?'); // pcm_sample_rate
    p.push(b'?'); // pcm_channels
    p.push(b'?'); // pcm_endian
    p.push(0); // threshold (KB) before autostart
    p.push(0); // spdif_enable
    p.push(0); // transition_period
    p.push(b'0'); // transition_type: none
    p.push(0); // flags
    p.push(0); // output_threshold
    p.push(0); // slaves_flag
    p.extend_from_slice(&0u32.to_be_bytes()); // replay_gain
    p.extend_from_slice(&server_port.to_be_bytes()); // server_port
    p.extend_from_slice(&0u32.to_be_bytes()); // server_ip = 0 → reuse control server
    // The HTTP request the player issues to fetch the stream.
    p.extend_from_slice(format!("GET {http_path} HTTP/1.0\r\n\r\n").as_bytes());
    ServerMessage::Strm {
        command: b's',
        payload: p,
    }
}

/// Build a simple `strm` control message (pause `p`, unpause `u`, stop `q`).
/// These carry the same 23-byte fixed header (zeroed) as the status query.
pub fn strm_control(command: u8) -> ServerMessage {
    ServerMessage::Strm {
        command,
        payload: vec![0u8; 23],
    }
}

// ---------------------------------------------------------------------------
// Player registry
// ---------------------------------------------------------------------------

/// Why the last native SlimProto playback stopped or degraded.  This is kept
/// separate from `ended_naturally`: an underrun or a broken stream must never
/// advance the queue as if the renderer had drained a complete track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlimProtoPlaybackFailure {
    Underrun,
    OutputUnderrun,
    UnsupportedFormat,
    StreamDisconnected(u8),
    ControlDisconnected,
}

impl SlimProtoPlaybackFailure {
    pub(crate) fn diagnostic(self) -> String {
        match self {
            Self::Underrun => "underrun".into(),
            Self::OutputUnderrun => "output_underrun".into(),
            Self::UnsupportedFormat => "unsupported_format".into(),
            Self::StreamDisconnected(reason) => format!("stream_disconnected:{reason}"),
            Self::ControlDisconnected => "control_disconnected".into(),
        }
    }
}

/// Functional playback state shared by the TCP reader and the registered
/// output.  Commands provide an immediate pending state; STAT then becomes the
/// source of truth for start, pause, decoder completion and final drain.
#[derive(Debug)]
pub(crate) struct SlimProtoPlaybackState {
    pub(crate) transport: TransportState,
    pub(crate) decoder_finished: bool,
    pub(crate) ended_naturally: bool,
    pub(crate) failure: Option<SlimProtoPlaybackFailure>,
}

impl Default for SlimProtoPlaybackState {
    fn default() -> Self {
        Self {
            transport: TransportState::Stopped,
            decoder_finished: false,
            ended_naturally: false,
            failure: None,
        }
    }
}

impl SlimProtoPlaybackState {
    pub(crate) fn begin_playback(&mut self) {
        self.transport = TransportState::Transitioning;
        self.decoder_finished = false;
        self.ended_naturally = false;
        self.failure = None;
    }

    pub(crate) fn pause(&mut self) {
        self.transport = TransportState::Paused;
    }

    pub(crate) fn resume(&mut self) {
        self.transport = TransportState::Playing;
    }

    pub(crate) fn stop(&mut self) {
        self.transport = TransportState::Stopped;
        self.decoder_finished = false;
        self.ended_naturally = false;
        self.failure = None;
    }

    /// Apply a player STAT event.
    ///
    /// Squeezelite emits `STMd` when the decoder has no more input, while PCM
    /// may still remain in its output buffer.  The later terminal `STMu` is a
    /// natural end only when that decoder-complete marker was observed first;
    /// an isolated `STMu` is a real underrun and stays fail-closed.
    pub(crate) fn apply_stat(&mut self, event: [u8; 4]) {
        match &event {
            b"STMc" | b"STMe" | b"STMh" | b"STMl" | b"STMa" => {
                if self.transport != TransportState::Stopped {
                    self.transport = TransportState::Transitioning;
                }
            }
            b"STMs" => {
                self.transport = TransportState::Playing;
                self.decoder_finished = false;
                self.ended_naturally = false;
                self.failure = None;
            }
            b"STMp" => self.transport = TransportState::Paused,
            b"STMr" => self.transport = TransportState::Playing,
            b"STMd" => self.decoder_finished = true,
            b"STMu" => {
                self.transport = TransportState::Stopped;
                self.ended_naturally = self.decoder_finished;
                if !self.decoder_finished {
                    self.failure = Some(SlimProtoPlaybackFailure::Underrun);
                }
            }
            b"STMo" => {
                self.failure = Some(SlimProtoPlaybackFailure::OutputUnderrun);
            }
            b"STMn" => {
                self.transport = TransportState::Stopped;
                self.ended_naturally = false;
                self.failure = Some(SlimProtoPlaybackFailure::UnsupportedFormat);
            }
            // A start command itself begins with a decoder/output flush.  Keep
            // its pending state, but preserve an explicit local Stop.
            b"STMf" => {
                if self.transport != TransportState::Stopped {
                    self.transport = TransportState::Transitioning;
                }
                self.decoder_finished = false;
                self.ended_naturally = false;
            }
            // `STMt` is a heartbeat/position sample and must not manufacture a
            // transport transition.
            _ => {}
        }
    }

    pub(crate) fn stream_disconnected(&mut self, reason: u8) {
        // reason=0 is Squeezelite's normal source EOF.  Its decoded PCM can
        // still be draining, so only the later STMu may stop it naturally.
        if reason == 0 {
            return;
        }
        self.transport = TransportState::Stopped;
        self.decoder_finished = false;
        self.ended_naturally = false;
        self.failure = Some(SlimProtoPlaybackFailure::StreamDisconnected(reason));
    }

    pub(crate) fn control_disconnected(&mut self) {
        self.transport = TransportState::Stopped;
        self.decoder_finished = false;
        self.ended_naturally = false;
        self.failure = Some(SlimProtoPlaybackFailure::ControlDisconnected);
    }
}

pub(crate) type SlimProtoPlayback = Arc<StdMutex<SlimProtoPlaybackState>>;

pub(crate) fn new_playback_state() -> SlimProtoPlayback {
    Arc::new(StdMutex::new(SlimProtoPlaybackState::default()))
}

pub(crate) fn lock_playback(
    playback: &SlimProtoPlayback,
) -> StdMutexGuard<'_, SlimProtoPlaybackState> {
    playback
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Format a MAC address as colon-separated hex string.
fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// A connected Squeezelite player.
#[derive(Debug)]
pub struct SlimProtoPlayer {
    /// MAC address (6 bytes).
    pub mac: [u8; 6],
    /// Human-readable MAC string (e.g. "aa:bb:cc:dd:ee:ff").
    pub mac_str: String,
    /// Player-reported device name (from HELO).
    pub name: String,
    /// Remote IP address.
    pub addr: SocketAddr,
    /// Device type byte from HELO.
    pub device_type: u8,
    /// Firmware version byte from HELO.
    pub firmware_version: u8,
    /// Last time we received a STAT from this player.
    pub last_stat: Instant,
    /// Last reported elapsed time in milliseconds.
    pub elapsed_ms: u32,
    /// Last reported bytes received.
    pub bytes_received: u64,
    /// Last STAT event code (e.g. `STMt` timer, `STMd` decoder-ready/track-end,
    /// `STMu` underrun). Kept for diagnostics and end-of-track heuristics.
    pub last_event: [u8; 4],
    /// State shared with the native output registered for this player.
    pub(crate) playback: SlimProtoPlayback,
}

/// Thread-safe registry of connected players, keyed by MAC string.
pub type PlayerRegistry = Arc<Mutex<HashMap<String, SlimProtoPlayer>>>;

/// Create a new empty player registry.
pub fn new_player_registry() -> PlayerRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// SlimProto TCP server
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Ce que devient le port 3483 quand on n'arrive pas a le prendre (#2938)
// ---------------------------------------------------------------------------

/// Delai laisse a la sonde pour joindre celui qui tient le port.
///
/// Une seconde suffit largement en boucle locale ; au-dela, on prefere rendre
/// « indeterminee » plutot que de retarder le demarrage du serveur.
const DELAI_SONDE_PORT: std::time::Duration = std::time::Duration::from_secs(1);

/// Ce que la sonde a pu ETABLIR sur un port que le noyau nous a refuse.
///
/// Le ticket #2938 releve cinq journaux de testeurs, sur deux systemes, ou le
/// bind de 3483 echoue — et rien dans ces journaux ne permet de departager les
/// causes. Cette enumeration est ce qu'une seule mesure, faite au moment de
/// l'echec, permet de trancher : on essaie de se CONNECTER au port. Le
/// resultat separe un conflit franc (quelqu'un ecoute) d'un refus du systeme
/// (personne n'ecoute, et pourtant le bind est refuse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CausePortIndisponible {
    /// Quelqu'un accepte des connexions sur ce port : conflit franc avec un
    /// autre serveur (Lyrion/LMS, ou une instance precedente de Tune).
    UnAutreServeurEcoute,
    /// Personne n'accepte de connexion, et pourtant le bind est refuse. Sous
    /// Windows c'est la signature d'une plage de ports exclue par le systeme
    /// (Hyper-V / WinNAT reservent des blocs entiers) ; ailleurs, d'un socket
    /// lie a une adresse precise que la boucle locale ne voit pas.
    PersonneNEcoute,
    /// La sonde n'a pas pu conclure (delai depasse, pare-feu qui absorbe).
    Indeterminee,
}

impl CausePortIndisponible {
    /// Le code machine, pour la route de diagnostic et l'evenement.
    pub fn code(self) -> &'static str {
        match self {
            Self::UnAutreServeurEcoute => "port_tenu_par_un_autre_serveur",
            Self::PersonneNEcoute => "port_refuse_par_le_systeme",
            Self::Indeterminee => "cause_indeterminee",
        }
    }

    /// La phrase que lira un testeur : la cause, la consequence, le
    /// contournement. Meme forme que le message du repondeur UDP voisin.
    pub fn phrase(self, port: u16) -> String {
        match self {
            Self::UnAutreServeurEcoute => format!(
                "un autre serveur ecoute deja sur le port TCP {port} (un Lyrion/LMS \
                 installe sur cette machine, ou une instance precedente de Tune non \
                 terminee) : les platines Squeezebox ne pourront pas se connecter a \
                 Tune. Arretez l'autre serveur, ou donnez un autre port a Tune avec \
                 la variable TUNE_SLIMPROTO_PORT."
            ),
            Self::PersonneNEcoute => format!(
                "le port TCP {port} est refuse par le systeme alors que PERSONNE n'y \
                 ecoute (sous Windows : plage de ports exclue par Hyper-V/WinNAT — \
                 « netsh int ipv4 show excludedportrange tcp ») : les platines \
                 Squeezebox ne pourront pas se connecter a Tune. Donnez un autre port \
                 a Tune avec la variable TUNE_SLIMPROTO_PORT."
            ),
            Self::Indeterminee => format!(
                "le port TCP {port} est refuse et la sonde n'a pas pu joindre celui \
                 qui le tient : les platines Squeezebox ne pourront pas se connecter a \
                 Tune. Verifiez qui tient le port (« ss -lptn 'sport = :{port}' » sous \
                 Linux, « netstat -ano | findstr :{port} » sous Windows), ou donnez un \
                 autre port a Tune avec la variable TUNE_SLIMPROTO_PORT."
            ),
        }
    }
}

/// L'etat du canal TCP de SlimProto, retenu pour toute la session.
///
/// C'est la moitie du ticket qui coute le plus cher au testeur : sans cet
/// etat, l'echec du bind ne vivait que dans UNE ligne de journal, dans une
/// tache detachee, sans route ni ecran pour la relire. Un `bus.emit` seul n'y
/// suffit pas : le bus est un `broadcast` sans rejeu, et le bind a lieu au
/// DEMARRAGE — aucun client WebSocket n'est encore connecte, l'evenement ne
/// serait recu par personne. L'evenement part quand meme (il sert a une
/// tentative faite en cours de session), mais c'est cet etat-ci qui survit et
/// que `/system/diagnostics/network` sert.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EtatEcouteSlimProto {
    /// Le port sur lequel l'ecoute a ete tentee.
    pub port: u16,
    /// `true` si le bind a reussi et que le serveur accepte des connexions.
    pub ecoute: bool,
    /// Code machine de la cause, `None` quand l'ecoute est en service.
    pub cause: Option<&'static str>,
    /// Phrase lisible, `None` quand l'ecoute est en service.
    pub message: Option<String>,
    /// L'erreur du systeme, telle quelle (`os error 98`, `os error 10048`…).
    pub erreur_systeme: Option<String>,
}

static ETAT_ECOUTE: std::sync::RwLock<Option<EtatEcouteSlimProto>> = std::sync::RwLock::new(None);

/// L'etat du canal TCP SlimProto, ou `None` tant qu'aucune tentative d'ecoute
/// n'a eu lieu. Lu par `/system/diagnostics/network` et par le rapport de bogue.
pub fn etat_ecoute() -> Option<EtatEcouteSlimProto> {
    ETAT_ECOUTE.read().ok().and_then(|g| g.clone())
}

fn retenir_etat_ecoute(etat: EtatEcouteSlimProto) {
    if let Ok(mut g) = ETAT_ECOUTE.write() {
        *g = Some(etat);
    }
}

/// Essaie de se connecter au port pour savoir QUI le tient.
///
/// On sonde la boucle locale d'abord ; puis, si on la connait, l'adresse LAN du
/// serveur — un socket lie a `192.168.x.y:3483` seul refuse notre bind sur
/// `0.0.0.0` sans jamais repondre sur `127.0.0.1`.
async fn sonder_qui_tient_le_port(port: u16, adresses: &[String]) -> CausePortIndisponible {
    let mut indeterminee = false;
    for hote in adresses {
        let Ok(addr) = format!("{hote}:{port}").parse::<SocketAddr>() else {
            continue;
        };
        match tokio::time::timeout(DELAI_SONDE_PORT, TcpStream::connect(addr)).await {
            Ok(Ok(_)) => return CausePortIndisponible::UnAutreServeurEcoute,
            Ok(Err(_)) => {}
            Err(_) => indeterminee = true,
        }
    }
    if indeterminee {
        CausePortIndisponible::Indeterminee
    } else {
        CausePortIndisponible::PersonneNEcoute
    }
}

/// Server state needed to bridge a connected player into a Tune zone + playback.
/// Optional so the server can still be constructed bare in unit tests.
pub struct SlimProtoState {
    pub db: Arc<dyn crate::db::backend::DbBackend>,
    pub event_bus: Arc<crate::event_bus::EventBus>,
    pub outputs: Arc<Mutex<crate::outputs::OutputRegistry>>,
    /// Local server IP advertised to players in the `strm s` HTTP request.
    pub server_ip: String,
    /// Per-player command senders (keyed by MAC) so [`crate::outputs::slimproto::SlimProtoOutput`]
    /// can push `strm`/`audg` commands into a specific connected player's writer task.
    pub command_channels: CommandChannels,
}

/// Map of connected player MAC → command sender into that player's writer task.
pub type CommandChannels = Arc<Mutex<HashMap<String, tokio::sync::mpsc::Sender<ServerMessage>>>>;

/// The SlimProto TCP server that accepts connections from Squeezelite players.
pub struct SlimProtoServer {
    port: u16,
    players: PlayerRegistry,
    /// Zone/playback bridge state. `None` in unit tests (server accepts
    /// connections but does not register zones).
    state: Option<Arc<SlimProtoState>>,
    /// Le bus, tenu a part de [`SlimProtoState`] : l'annonce d'un echec de bind
    /// doit pouvoir partir d'un serveur qui n'a AUCUN pont de zone (#2938).
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
}

impl SlimProtoServer {
    /// Create a new server. The port defaults to 3483 but can be overridden
    /// via the `TUNE_SLIMPROTO_PORT` environment variable. No zone bridging
    /// (used by unit tests) — prefer [`new_with_state`] in production.
    pub fn new() -> Self {
        Self {
            port: Self::resolve_port(),
            players: new_player_registry(),
            state: None,
            event_bus: None,
        }
    }

    /// Meme chose, mais sur un port impose plutot que resolu depuis
    /// l'environnement. Sert a une mesure qui doit choisir son port (le port 0
    /// laisse le systeme en attribuer un libre) sans toucher a
    /// `TUNE_SLIMPROTO_PORT` : une variable d'environnement posee dans un test
    /// contamine toute la suite.
    pub fn new_sur_port(port: u16) -> Self {
        Self {
            port,
            players: new_player_registry(),
            state: None,
            event_bus: None,
        }
    }

    /// Branche le bus d'evenements sur un serveur construit sans pont de zone.
    pub fn avec_bus(mut self, bus: Arc<crate::event_bus::EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Create a server wired to the app state so connected players are
    /// registered as zones and can be driven for playback.
    pub fn new_with_state(
        db: Arc<dyn crate::db::backend::DbBackend>,
        event_bus: Arc<crate::event_bus::EventBus>,
        outputs: Arc<Mutex<crate::outputs::OutputRegistry>>,
        server_ip: String,
    ) -> Self {
        Self {
            port: Self::resolve_port(),
            players: new_player_registry(),
            state: Some(Arc::new(SlimProtoState {
                db,
                event_bus: event_bus.clone(),
                outputs,
                server_ip,
                command_channels: Arc::new(Mutex::new(HashMap::new())),
            })),
            event_bus: Some(event_bus),
        }
    }

    fn resolve_port() -> u16 {
        std::env::var("TUNE_SLIMPROTO_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PORT)
    }

    /// Return a reference to the player registry (for use by other subsystems).
    pub fn players(&self) -> &PlayerRegistry {
        &self.players
    }

    /// Start listening and spawn per-client handlers. This runs forever.
    ///
    /// Un bind refuse ne fait plus qu'une ligne de journal noyee (#2938) : la
    /// cause est SONDEE, nommee, retenue dans [`etat_ecoute`] pour toute la
    /// session, et annoncee sur le bus. L'appelant garde une `Err` — le serveur
    /// HTTP, lui, demarre quand meme : `tune-server/src/background.rs` detache
    /// cette tache, un SlimProto mort reste un service degrade, jamais un
    /// serveur mort.
    pub async fn spawn(self: Arc<Self>) -> Result<(), String> {
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => return Err(self.annoncer_bind_impossible(&addr, &e).await),
        };

        // Le port reellement obtenu : avec `port = 0` le systeme en attribue un,
        // et l'etat doit dire celui-la, pas le zero demande.
        let port_obtenu = listener.local_addr().map(|a| a.port()).unwrap_or(self.port);
        retenir_etat_ecoute(EtatEcouteSlimProto {
            port: port_obtenu,
            ecoute: true,
            cause: None,
            message: None,
            erreur_systeme: None,
        });
        info!(port = port_obtenu, "slimproto_server_started");

        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    info!(peer = %peer, "slimproto_client_connected");
                    let server = Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(e) = server.handle_client(stream, peer).await {
                            debug!(peer = %peer, error = %e, "slimproto_client_handler_error");
                        }
                        info!(peer = %peer, "slimproto_client_disconnected");
                    });
                }
                Err(e) => {
                    warn!(error = %e, "slimproto_accept_error");
                }
            }
        }
    }

    /// Sonde, nomme, retient et annonce un bind refuse. Rend le texte d'erreur
    /// que l'appelant propage.
    ///
    /// Les trois sorties ne font qu'une seule mesure : la sonde de port. Elles
    /// disent la meme chose a trois lecteurs differents — le journal pour qui
    /// l'exporte, [`etat_ecoute`] pour la route de diagnostic et le rapport de
    /// bogue, le bus pour un client deja connecte.
    async fn annoncer_bind_impossible(&self, addr: &str, e: &std::io::Error) -> String {
        let mut adresses = vec!["127.0.0.1".to_string()];
        if let Some(ip) = self.state.as_ref().map(|s| s.server_ip.clone())
            && ip != "127.0.0.1"
        {
            adresses.push(ip);
        }
        let cause = sonder_qui_tient_le_port(self.port, &adresses).await;
        let phrase = cause.phrase(self.port);

        error!(
            port = self.port,
            cause = cause.code(),
            error = %e,
            "slimproto_bind_impossible — {phrase}"
        );

        retenir_etat_ecoute(EtatEcouteSlimProto {
            port: self.port,
            ecoute: false,
            cause: Some(cause.code()),
            message: Some(phrase.clone()),
            erreur_systeme: Some(e.to_string()),
        });

        if let Some(bus) = self.event_bus.as_ref() {
            bus.emit_typed(
                crate::event_types::EventType::SlimprotoListenFailed,
                serde_json::json!({
                    "port": self.port,
                    "cause": cause.code(),
                    "message": phrase,
                    "erreur_systeme": e.to_string(),
                }),
            );
        }

        format!("slimproto bind {addr}: {e} — {phrase}")
    }

    /// Handle a single client connection.
    async fn handle_client(&self, mut stream: TcpStream, peer: SocketAddr) -> Result<(), String> {
        // Non-destructively peek the first bytes so an unusual handshake framing
        // is visible in the log. Tune expects `[len:2][tag:4][payload]`; if a
        // client (Progman's slim2diretta) uses a different framing, read_message
        // would misread the length and block. Logging the raw hex/ASCII of the
        // first bytes lets us identify the actual framing from a user's log.
        {
            let mut peek_buf = [0u8; 16];
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                stream.peek(&mut peek_buf),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => {
                    let hex: String = peek_buf[..n]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let ascii: String = peek_buf[..n]
                        .iter()
                        .map(|&b| {
                            if (0x20..0x7f).contains(&b) {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    info!(peer = %peer, bytes = n, hex = %hex, ascii = %ascii, "slimproto_first_bytes");
                }
                Ok(Ok(_)) => {
                    warn!(peer = %peer, "slimproto_peer_closed_before_handshake");
                }
                Ok(Err(e)) => {
                    warn!(peer = %peer, error = %e, "slimproto_peek_failed");
                }
                Err(_) => {
                    warn!(peer = %peer, "slimproto_no_bytes_within_10s — client connected but sent nothing");
                }
            }
        }

        // The first message from a Squeezelite client should be HELO. Bound the
        // read: a client that connects but never sends a parseable HELO (or uses
        // a different framing) would otherwise hang read_message forever with no
        // log and never register a zone (Progman's slim2diretta — TCP connects,
        // then silence). Time out so the issue surfaces and the socket is freed.
        let first_msg = match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            read_message(&mut stream),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                warn!(
                    peer = %peer,
                    "slimproto_helo_timeout — no HELO within 15s (client connected but sent no parseable handshake)"
                );
                return Err("HELO read timed out".into());
            }
        };
        let mac_str = match first_msg {
            ClientMessage::Helo {
                device_type,
                firmware_version,
                mac,
                ref name,
            } => {
                let mac_str = format_mac(&mac);
                let player_name = if name.is_empty() {
                    format!("Squeezelite {}", &mac_str[9..]) // last 3 octets
                } else {
                    name.clone()
                };

                info!(
                    mac = %mac_str,
                    name = %player_name,
                    device_type,
                    firmware_version,
                    peer = %peer,
                    "slimproto_helo_received"
                );

                // Register the player.
                {
                    let playback = new_playback_state();
                    let mut players = self.players.lock().await;
                    players.insert(
                        mac_str.clone(),
                        SlimProtoPlayer {
                            mac,
                            mac_str: mac_str.clone(),
                            name: player_name,
                            addr: peer,
                            device_type,
                            firmware_version,
                            last_stat: Instant::now(),
                            elapsed_ms: 0,
                            bytes_received: 0,
                            last_event: [0u8; 4],
                            playback,
                        },
                    );
                }

                mac_str
            }
            other => {
                warn!(
                    peer = %peer,
                    msg = ?other,
                    "slimproto_expected_helo_got_something_else"
                );
                return Err("expected HELO as first message".into());
            }
        };

        // Bridge the connected player into a Tune zone + register its output so
        // it appears in the UI and can be selected for playback.
        self.register_player_zone(&mac_str).await;

        // Spawn a heartbeat task that sends `strm t` periodically.
        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<ServerMessage>(16);

        // Publish this player's command channel so its output can push
        // strm/audg commands to the writer task. Uses the same channel the
        // heartbeat drains (the writer serialises both).
        if let Some(state) = self.state.clone() {
            state
                .command_channels
                .lock()
                .await
                .insert(mac_str.clone(), heartbeat_tx.clone());
        }

        let heartbeat_handle = {
            let tx = heartbeat_tx.clone();
            tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
                loop {
                    interval.tick().await;
                    if tx.send(strm_status_query()).await.is_err() {
                        break; // channel closed, client gone
                    }
                }
            })
        };

        // Split the TCP stream for concurrent read/write.
        let (reader, writer) = stream.into_split();
        let reader = Arc::new(Mutex::new(reader));
        let writer = Arc::new(Mutex::new(writer));

        // Writer task: drains heartbeat_rx and sends messages to the player.
        let writer_clone = Arc::clone(&writer);
        let mac_for_writer = mac_str.clone();
        let writer_handle = tokio::spawn(async move {
            while let Some(msg) = heartbeat_rx.recv().await {
                let mut w = writer_clone.lock().await;
                // Reassemble a TcpStream is not possible with split halves,
                // so we write directly to the write half.
                if let Err(e) = write_message_to_writer(&mut *w, &msg).await {
                    debug!(mac = %mac_for_writer, error = %e, "slimproto_write_failed");
                    break;
                }
            }
        });

        // Reader loop: read messages from the player until disconnect.
        let players = Arc::clone(&self.players);
        let mac_for_reader = mac_str.clone();
        let reader_result: Result<(), String> = {
            loop {
                let msg = {
                    let mut r = reader.lock().await;
                    read_message_from_reader(&mut *r).await
                };

                match msg {
                    Ok(ClientMessage::Stat {
                        event,
                        bytes_received,
                        elapsed_ms,
                        signal_strength,
                        ..
                    }) => {
                        let event_str = String::from_utf8_lossy(&event);
                        debug!(
                            mac = %mac_for_reader,
                            event = %event_str,
                            elapsed_ms,
                            bytes_received,
                            signal_strength,
                            "slimproto_stat"
                        );

                        // Update player state.
                        let mut reg = players.lock().await;
                        if let Some(player) = reg.get_mut(&mac_for_reader) {
                            player.last_stat = Instant::now();
                            player.elapsed_ms = elapsed_ms;
                            player.bytes_received = bytes_received;
                            player.last_event = event;
                            lock_playback(&player.playback).apply_stat(event);
                        }
                    }
                    Ok(ClientMessage::Bye) => {
                        info!(mac = %mac_for_reader, "slimproto_bye_received");
                        if let Some(player) = players.lock().await.get(&mac_for_reader) {
                            lock_playback(&player.playback).control_disconnected();
                        }
                        break Ok(());
                    }
                    Ok(ClientMessage::Dsco { reason }) => {
                        info!(mac = %mac_for_reader, reason, "slimproto_dsco_received");
                        // Player disconnected from the audio stream — not from us.
                        // Stay connected and keep heartbeating.
                        if let Some(player) = players.lock().await.get(&mac_for_reader) {
                            lock_playback(&player.playback).stream_disconnected(reason);
                        }
                    }
                    Ok(ClientMessage::Resp { data }) => {
                        debug!(
                            mac = %mac_for_reader,
                            len = data.len(),
                            "slimproto_resp_received"
                        );
                    }
                    Ok(ClientMessage::Meta { data }) => {
                        debug!(
                            mac = %mac_for_reader,
                            len = data.len(),
                            "slimproto_meta_received"
                        );
                    }
                    Ok(ClientMessage::Helo { .. }) => {
                        warn!(mac = %mac_for_reader, "slimproto_duplicate_helo");
                    }
                    Ok(ClientMessage::Unknown { tag, data }) => {
                        debug!(
                            mac = %mac_for_reader,
                            tag = %String::from_utf8_lossy(&tag),
                            len = data.len(),
                            "slimproto_unknown_msg"
                        );
                    }
                    Err(e) => {
                        // Connection closed or read error.
                        debug!(mac = %mac_for_reader, error = %e, "slimproto_read_error");
                        if let Some(player) = players.lock().await.get(&mac_for_reader) {
                            lock_playback(&player.playback).control_disconnected();
                        }
                        break Err(e);
                    }
                }
            }
        };

        // Cleanup: abort heartbeat, close writer channel.
        heartbeat_handle.abort();
        drop(heartbeat_tx);
        writer_handle.abort();

        // Mark the zone offline and drop its output before removing the player.
        self.unregister_player_zone(&mac_str).await;

        // Unregister the player.
        {
            let mut reg = self.players.lock().await;
            if let Some(player) = reg.remove(&mac_str) {
                info!(
                    mac = %mac_str,
                    name = %player.name,
                    "slimproto_player_unregistered"
                );
            }
        }

        reader_result
    }

    /// Create (or online) a Tune zone for a connected player and register its
    /// native SlimProto output. No-op when the server has no app state (tests)
    /// or when the zone was soft-deleted by the user (respects `is_hidden`).
    async fn register_player_zone(&self, mac_str: &str) {
        let Some(state) = self.state.clone() else {
            return;
        };
        let device_id = format!("slimproto-{mac_str}");
        let (player_name, playback) = {
            let reg = self.players.lock().await;
            match reg.get(mac_str) {
                Some(p) => (p.name.clone(), Arc::clone(&p.playback)),
                None => return,
            }
        };

        let zone_repo = crate::db::zone_repo::ZoneRepo::with_backend(state.db.clone());
        // Respect a user deletion: a hidden zone must not reappear on reconnect.
        if zone_repo.is_device_hidden(&device_id) {
            debug!(mac = %mac_str, "slimproto_zone_hidden_skipping");
            return;
        }

        match zone_repo.get_or_create(&player_name, Some("slimproto"), &device_id) {
            Ok((zone_id, created)) => {
                if created {
                    state
                        .event_bus
                        .emit_typed(crate::event_types::EventType::ZoneCreated, {
                            // Meme contrat que la route API et la decouverte :
                            // le client teste `data.zone` avant de fusionner, et
                            // attend le volume en 0..1 (#2224).
                            let mut charge = serde_json::json!({
                                "zone_id": zone_id,
                                "name": player_name.clone(),
                                "device_id": device_id.clone(),
                                "type": "slimproto",
                                "id": zone_id,
                            });
                            if let Some(obj) = charge.as_object_mut()
                                && let Ok(Some(z)) = zone_repo.get(zone_id)
                            {
                                obj.insert(
                                    "zone".into(),
                                    crate::db::zone_repo::zone_creee_contrat_client(
                                        Some(&z),
                                        zone_id,
                                        &player_name,
                                    ),
                                );
                            }
                            charge
                        });
                } else {
                    let _ = zone_repo.set_online_by_device(&device_id, true);
                    state.event_bus.emit_typed(
                        crate::event_types::EventType::ZoneUpdated,
                        serde_json::json!({ "device_id": device_id.clone(), "online": true }),
                    );
                }
                info!(mac = %mac_str, zone_id, device_id = %device_id, "slimproto_zone_registered");
            }
            Err(e) => warn!(mac = %mac_str, error = %e, "slimproto_zone_create_failed"),
        }

        // Register the native output so the orchestrator can route to it.
        let output = crate::outputs::slimproto::SlimProtoOutput::new_with_playback(
            player_name,
            device_id,
            mac_str.to_string(),
            Arc::clone(&self.players),
            Arc::clone(&state.command_channels),
            playback,
        );
        state.outputs.lock().await.register(Box::new(output));
    }

    /// Mark the zone offline and remove its output when a player disconnects.
    async fn unregister_player_zone(&self, mac_str: &str) {
        let Some(state) = self.state.clone() else {
            return;
        };
        let device_id = format!("slimproto-{mac_str}");
        let zone_repo = crate::db::zone_repo::ZoneRepo::with_backend(state.db.clone());
        let _ = zone_repo.set_online_by_device(&device_id, false);
        state.event_bus.emit_typed(
            crate::event_types::EventType::ZoneUpdated,
            serde_json::json!({ "device_id": device_id.clone(), "online": false }),
        );
        state.outputs.lock().await.remove(&device_id);
        state.command_channels.lock().await.remove(mac_str);
        info!(mac = %mac_str, device_id = %device_id, "slimproto_zone_offline");
    }
}

// ---------------------------------------------------------------------------
// Read/write helpers for split stream halves
// ---------------------------------------------------------------------------

/// Read one client→server message from a `ReadHalf`.
async fn read_message_from_reader(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
) -> Result<ClientMessage, String> {
    use tokio::io::AsyncReadExt;

    // Client -> server keeps the same framing after `into_split()` as the
    // initial HELO: tag first, then a 32-bit payload length.  The old helper
    // accidentally used the opposite server -> client 16-bit framing, making
    // every post-HELO STAT unreadable.
    let mut tag = [0u8; 4];
    reader
        .read_exact(&mut tag)
        .await
        .map_err(|e| format!("read tag: {e}"))?;

    let payload_len = reader
        .read_u32()
        .await
        .map_err(|e| format!("read length: {e}"))? as usize;
    if payload_len > MAX_MESSAGE_LEN {
        return Err(format!("payload too large: {payload_len} bytes"));
    }

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader
            .read_exact(&mut payload)
            .await
            .map_err(|e| format!("read payload ({payload_len} bytes): {e}"))?;
    }

    debug!(
        tag = %String::from_utf8_lossy(&tag),
        payload_len,
        "slimproto_msg_received"
    );

    parse_client_message(tag, payload)
}

/// Write one server→client message to a `WriteHalf`.
async fn write_message_to_writer(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    msg: &ServerMessage,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let (tag, payload) = match msg {
        ServerMessage::Strm { command, payload } => {
            let mut buf = Vec::with_capacity(1 + payload.len());
            buf.push(*command);
            buf.extend_from_slice(payload);
            (*b"strm", buf)
        }
        ServerMessage::Audg {
            left_gain,
            right_gain,
            digital_volume,
        } => {
            let mut buf = Vec::with_capacity(18);
            buf.extend_from_slice(&left_gain.to_be_bytes());
            buf.extend_from_slice(&right_gain.to_be_bytes());
            buf.push(*digital_volume);
            buf.push(0);
            buf.extend_from_slice(&left_gain.to_be_bytes());
            buf.extend_from_slice(&right_gain.to_be_bytes());
            (*b"audg", buf)
        }
        ServerMessage::Setd { data } => (*b"setd", data.clone()),
        ServerMessage::Serv { data } => (*b"serv", data.clone()),
    };

    let total_len = (4 + payload.len()) as u16;

    debug!(
        tag = %String::from_utf8_lossy(&tag),
        payload_len = payload.len(),
        "slimproto_msg_sent"
    );

    writer
        .write_u16(total_len)
        .await
        .map_err(|e| format!("write length: {e}"))?;
    writer
        .write_all(&tag)
        .await
        .map_err(|e| format!("write tag: {e}"))?;
    if !payload.is_empty() {
        writer
            .write_all(&payload)
            .await
            .map_err(|e| format!("write payload: {e}"))?;
    }
    writer.flush().await.map_err(|e| format!("flush: {e}"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_mac_address() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        assert_eq!(format_mac(&mac), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn format_mac_zeros() {
        let mac = [0x00; 6];
        assert_eq!(format_mac(&mac), "00:00:00:00:00:00");
    }

    #[test]
    fn parse_helo_minimal() {
        // 2 bytes device_type + firmware, 6 bytes MAC, 2 bytes wlan_channels
        let payload = vec![
            10, // device_type
            5,  // firmware_version
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, // MAC
            0, 0, // wlan channels
        ];
        let msg = parse_client_message(*b"HELO", payload).unwrap();
        match msg {
            ClientMessage::Helo {
                device_type,
                firmware_version,
                mac,
                name,
            } => {
                assert_eq!(device_type, 10);
                assert_eq!(firmware_version, 5);
                assert_eq!(mac, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
                assert!(name.is_empty());
            }
            _ => panic!("expected Helo"),
        }
    }

    #[test]
    fn parse_helo_with_name() {
        let mut payload = vec![
            10, // device_type
            5,  // firmware_version
            0x00, 0x04, 0x20, 0x11, 0x22, 0x33, // MAC
            0, 0, // wlan channels
        ];
        payload.extend_from_slice(b"Living Room");
        let msg = parse_client_message(*b"HELO", payload).unwrap();
        match msg {
            ClientMessage::Helo { name, .. } => {
                assert_eq!(name, "Living Room");
            }
            _ => panic!("expected Helo"),
        }
    }

    #[test]
    fn parse_modern_helo_reads_model_name_after_binary_fields() {
        let mut payload = vec![
            12, // device_type
            0,  // firmware_version
            0x00, 0x04, 0x20, 0x2a, 0xe4, 0xfe, // MAC
        ];
        // UUID, WLAN channel bitmap, byte counter and language are deliberately
        // non-UTF-8/binary: none of them may leak into the display name.
        payload.extend_from_slice(&[
            0xff, 0x81, 0x00, 0x7f, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0,
            0xb0, 0xc0,
        ]);
        payload.extend_from_slice(&0x07ff_u16.to_be_bytes());
        payload.extend_from_slice(&21_700_000_u64.to_be_bytes());
        payload.extend_from_slice(b"FR");
        payload.extend_from_slice(
            b"Model=baby,ModelName=Squeezebox Radio,Firmware=8.0.1-r16924,alc,aac,ogg,flc",
        );

        let msg = parse_client_message(*b"HELO", payload).unwrap();
        match msg {
            ClientMessage::Helo { name, mac, .. } => {
                assert_eq!(name, "Squeezebox Radio");
                assert_eq!(mac, [0x00, 0x04, 0x20, 0x2a, 0xe4, 0xfe]);
                assert!(!name.contains('\u{fffd}'));
            }
            _ => panic!("expected Helo"),
        }
    }

    #[test]
    fn parse_modern_helo_falls_back_to_model_then_device_type() {
        let mut with_model = vec![0_u8; MODERN_HELO_BASE_LEN];
        with_model[0] = 12;
        with_model.extend_from_slice(b"Model=squeezelite,flc,pcm");
        let ClientMessage::Helo { name, .. } = parse_client_message(*b"HELO", with_model).unwrap()
        else {
            panic!("expected Helo");
        };
        assert_eq!(name, "squeezelite");

        let mut without_model = vec![0_u8; MODERN_HELO_BASE_LEN];
        without_model[0] = 10;
        without_model.extend_from_slice(b"flc,pcm");
        let ClientMessage::Helo { name, .. } =
            parse_client_message(*b"HELO", without_model).unwrap()
        else {
            panic!("expected Helo");
        };
        assert_eq!(name, "Squeezebox 10");
    }

    #[test]
    fn parse_bye() {
        let msg = parse_client_message(*b"BYE!", vec![]).unwrap();
        assert!(matches!(msg, ClientMessage::Bye));
    }

    #[test]
    fn parse_dsco() {
        let msg = parse_client_message(*b"DSCO", vec![2]).unwrap();
        match msg {
            ClientMessage::Dsco { reason } => assert_eq!(reason, 2),
            _ => panic!("expected Dsco"),
        }
    }

    #[test]
    fn parse_unknown_tag() {
        let msg = parse_client_message(*b"XYZW", vec![1, 2, 3]).unwrap();
        match msg {
            ClientMessage::Unknown { tag, data } => {
                assert_eq!(&tag, b"XYZW");
                assert_eq!(data, vec![1, 2, 3]);
            }
            _ => panic!("expected Unknown"),
        }
    }

    #[test]
    fn strm_status_query_builds() {
        let msg = strm_status_query();
        match msg {
            ServerMessage::Strm { command, payload } => {
                assert_eq!(command, b't');
                assert_eq!(payload.len(), 23);
            }
            _ => panic!("expected Strm"),
        }
    }

    #[test]
    fn default_port_is_3483() {
        // Without TUNE_SLIMPROTO_PORT set, the default port should be 3483.
        assert_eq!(DEFAULT_PORT, 3483);
    }

    #[test]
    fn parse_stat_basic() {
        // Build a minimal STAT payload: 4 bytes event + enough padding
        let mut payload = Vec::new();
        payload.extend_from_slice(b"STMt"); // event code
        // Pad to at least 39 bytes for elapsed_ms parsing
        payload.resize(53, 0);
        // Set elapsed_ms at bytes 35..39
        let elapsed: u32 = 12345;
        let elapsed_bytes = elapsed.to_be_bytes();
        payload[35] = elapsed_bytes[0];
        payload[36] = elapsed_bytes[1];
        payload[37] = elapsed_bytes[2];
        payload[38] = elapsed_bytes[3];

        let msg = parse_client_message(*b"STAT", payload).unwrap();
        match msg {
            ClientMessage::Stat {
                event, elapsed_ms, ..
            } => {
                assert_eq!(&event, b"STMt");
                assert_eq!(elapsed_ms, 12345);
            }
            _ => panic!("expected Stat"),
        }
    }

    #[tokio::test]
    async fn le_lecteur_post_helo_conserve_le_cadrage_client_stat() {
        use tokio::io::AsyncWriteExt as _;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut payload = vec![0; 53];
            payload[..4].copy_from_slice(b"STMt");
            payload[35..39].copy_from_slice(&12_345u32.to_be_bytes());
            stream.write_all(b"STAT").await.unwrap();
            stream.write_u32(payload.len() as u32).await.unwrap();
            stream.write_all(&payload).await.unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, _) = stream.into_split();
        let message = read_message_from_reader(&mut reader).await.unwrap();
        writer.await.unwrap();

        match message {
            ClientMessage::Stat {
                event, elapsed_ms, ..
            } => {
                assert_eq!(&event, b"STMt");
                assert_eq!(elapsed_ms, 12_345);
            }
            other => panic!("STAT attendu, reçu {other:?}"),
        }
    }

    #[test]
    fn stat_ne_termine_naturellement_qu_apres_decodage_puis_drainage() {
        let mut playback = SlimProtoPlaybackState::default();
        playback.begin_playback();
        assert_eq!(playback.transport, TransportState::Transitioning);

        playback.apply_stat(*b"STMs");
        assert_eq!(playback.transport, TransportState::Playing);
        playback.apply_stat(*b"STMt");
        assert_eq!(playback.transport, TransportState::Playing);

        playback.apply_stat(*b"STMp");
        assert_eq!(playback.transport, TransportState::Paused);
        playback.apply_stat(*b"STMr");
        assert_eq!(playback.transport, TransportState::Playing);

        playback.apply_stat(*b"STMd");
        assert!(playback.decoder_finished);
        assert_eq!(playback.transport, TransportState::Playing);
        assert!(!playback.ended_naturally);

        // DSCO(0) est l'EOF de la source HTTP, pas la fin du tampon audio.
        playback.stream_disconnected(0);
        assert_eq!(playback.transport, TransportState::Playing);
        assert!(!playback.ended_naturally);

        playback.apply_stat(*b"STMu");
        assert_eq!(playback.transport, TransportState::Stopped);
        assert!(playback.ended_naturally);
        assert_eq!(playback.failure, None);

        // Squeezelite protège déjà STMu avec `sentSTMu`; notre latch reste
        // idempotent si un lecteur tiers répète néanmoins le paquet.
        playback.apply_stat(*b"STMu");
        assert!(playback.ended_naturally);
    }

    #[test]
    fn underrun_et_deconnexion_restent_fail_closed() {
        let mut playback = SlimProtoPlaybackState::default();
        playback.begin_playback();
        playback.apply_stat(*b"STMs");
        playback.apply_stat(*b"STMu");
        assert_eq!(playback.transport, TransportState::Stopped);
        assert!(!playback.ended_naturally);
        assert_eq!(playback.failure, Some(SlimProtoPlaybackFailure::Underrun));

        playback.begin_playback();
        playback.apply_stat(*b"STMs");
        playback.stream_disconnected(2);
        assert_eq!(playback.transport, TransportState::Stopped);
        assert!(!playback.ended_naturally);
        assert_eq!(
            playback.failure,
            Some(SlimProtoPlaybackFailure::StreamDisconnected(2))
        );
    }

    #[tokio::test]
    async fn player_registry_insert_remove() {
        let registry = new_player_registry();
        let mac_str = "aa:bb:cc:dd:ee:ff".to_string();

        {
            let mut reg = registry.lock().await;
            reg.insert(
                mac_str.clone(),
                SlimProtoPlayer {
                    mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
                    mac_str: mac_str.clone(),
                    name: "Test Player".into(),
                    addr: "127.0.0.1:12345".parse().unwrap(),
                    device_type: 10,
                    firmware_version: 1,
                    last_stat: Instant::now(),
                    elapsed_ms: 0,
                    bytes_received: 0,
                    last_event: [0u8; 4],
                    playback: new_playback_state(),
                },
            );
            assert_eq!(reg.len(), 1);
        }

        {
            let mut reg = registry.lock().await;
            let removed = reg.remove(&mac_str);
            assert!(removed.is_some());
            assert!(reg.is_empty());
        }
    }

    // ── #2938 : un bind refuse doit NOMMER sa cause et la RETENIR ──────────
    //
    // Cinq testeurs, deux systemes, une seule ligne de journal en anglais dans
    // une tache detachee. Les mesures qui suivent portent sur le VRAI
    // `SlimProtoServer::spawn()` — pas sur une reecriture du mecanisme : c'est
    // la fonction que `tune-server/src/background.rs` appelle en production.

    /// L'etat d'ecoute est un global de processus. Deux mesures qui l'ecrivent
    /// en meme temps se marcheraient dessus : ce verrou les met a la file.
    fn verrou_etat() -> &'static tokio::sync::Mutex<()> {
        static V: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        V.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Attend que l'etat retenu satisfasse `predicat`, ou rend `None`.
    ///
    /// `spawn()` ne rend jamais la main quand l'ecoute reussit (c'est une
    /// boucle d'acceptation) : le succes se lit donc dans l'etat, pas dans une
    /// valeur de retour.
    async fn attendre_etat(
        predicat: impl Fn(&EtatEcouteSlimProto) -> bool,
    ) -> Option<EtatEcouteSlimProto> {
        for _ in 0..200 {
            if let Some(e) = etat_ecoute()
                && predicat(&e)
            {
                return Some(e);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    /// LA contre-epreuve. Sur un fait de base — un port reellement tenu par un
    /// autre socket — et jamais sur un code HTTP.
    ///
    /// Rouge avant le correctif : `spawn()` rendait « slimproto bind
    /// 0.0.0.0:P: Address already in use (os error 98) », rien d'autre
    /// n'existait, et `etat_ecoute()` n'existait pas du tout.
    #[tokio::test]
    async fn un_port_deja_pris_nomme_la_cause_la_retient_et_l_annonce() {
        let _verrou = verrou_etat().lock().await;

        // Le squatteur prend `0.0.0.0` : sous Linux, SO_REUSEADDR — que std
        // pose deja — laisse cohabiter `127.0.0.1:P` et `0.0.0.0:P`, mais
        // jamais deux fois la MEME adresse exacte. Port 0 : c'est le systeme
        // qui attribue, deux mesures concurrentes ne peuvent pas se disputer
        // le meme numero.
        let squatteur = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = squatteur.local_addr().unwrap().port();

        let bus = Arc::new(crate::event_bus::EventBus::new());
        let mut rx = bus.subscribe();
        let serveur = Arc::new(SlimProtoServer::new_sur_port(port).avec_bus(Arc::clone(&bus)));

        let err = serveur
            .spawn()
            .await
            .expect_err("le bind devait echouer : le port est tenu");

        // 1. Une trace LISIBLE : l'erreur propagee (celle que journalise
        //    `background.rs`) nomme la cause ET le contournement.
        assert!(
            err.contains("un autre serveur ecoute deja"),
            "l'erreur doit NOMMER ce qui tient le port, pas seulement « address \
             already in use » — obtenu : {err}"
        );
        assert!(
            err.contains("TUNE_SLIMPROTO_PORT"),
            "l'erreur doit donner le contournement — obtenu : {err}"
        );

        // 2. Un ETAT que quelque chose peut lire APRES coup : c'est lui que
        //    sert `/system/diagnostics/network` et le rapport de bogue. La
        //    ligne de journal, elle, est deja passee.
        let etat = etat_ecoute().expect("aucun etat retenu apres un bind refuse");
        assert_eq!(etat.port, port);
        assert!(!etat.ecoute, "l'etat doit dire que SlimProto n'ecoute pas");
        assert_eq!(
            etat.cause,
            Some("port_tenu_par_un_autre_serveur"),
            "la sonde a joint quelqu'un sur ce port : la cause doit le dire"
        );
        let message = etat.message.expect("l'etat doit porter une phrase lisible");
        assert!(
            message.contains("Squeezebox"),
            "la phrase doit dire la CONSEQUENCE pour l'utilisateur — obtenu : {message}"
        );
        assert!(
            message.contains("TUNE_SLIMPROTO_PORT"),
            "la phrase doit donner le contournement — obtenu : {message}"
        );
        assert!(
            etat.erreur_systeme.is_some(),
            "l'erreur brute du systeme (os error 98 / 10048) doit rester lisible"
        );

        // 3. Et l'annonce part sur le bus, pour un client deja connecte.
        let ev = rx
            .try_recv()
            .expect("aucun evenement annonce sur le bus apres un bind refuse");
        assert_eq!(ev.event_type, "slimproto.listen_failed");
        assert_eq!(ev.data["cause"], "port_tenu_par_un_autre_serveur");
        assert_eq!(ev.data["port"], port);

        drop(squatteur);
    }

    /// Temoin vert : un bind qui REUSSIT ne change rien. Aucune cause, aucun
    /// message, aucun evenement — et le port accepte vraiment une connexion.
    #[tokio::test]
    async fn un_bind_qui_reussit_n_annonce_aucun_echec() {
        let _verrou = verrou_etat().lock().await;

        let sonde = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = sonde.local_addr().unwrap().port();
        drop(sonde);

        let bus = Arc::new(crate::event_bus::EventBus::new());
        let mut rx = bus.subscribe();
        let serveur = Arc::new(SlimProtoServer::new_sur_port(port).avec_bus(Arc::clone(&bus)));
        let tache = tokio::spawn(Arc::clone(&serveur).spawn());

        let etat = attendre_etat(|e| e.port == port && e.ecoute)
            .await
            .expect("le bind devait reussir sur un port libre");
        assert!(etat.cause.is_none(), "aucune cause sur un bind qui reussit");
        assert!(
            etat.message.is_none(),
            "aucun message sur un bind qui reussit"
        );
        assert!(etat.erreur_systeme.is_none());

        // Le port accepte reellement : l'etat ne ment pas.
        TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("le serveur annonce ecouter mais refuse la connexion");

        assert!(
            rx.try_recv().is_err(),
            "un bind qui reussit ne doit annoncer AUCUNE erreur"
        );

        tache.abort();
    }

    /// Temoin vert : le sous-systeme n'est pas empoisonne par un premier echec.
    /// Une seconde tentative, le port une fois libere, ecoute — et l'etat
    /// retenu repasse au vert.
    #[tokio::test]
    async fn une_seconde_tentative_apres_liberation_du_port_reussit() {
        let _verrou = verrou_etat().lock().await;

        let squatteur = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = squatteur.local_addr().unwrap().port();

        let serveur = Arc::new(SlimProtoServer::new_sur_port(port));
        // Premiere tentative : refusee, et `spawn()` REND une erreur — il ne
        // panique pas et n'arrete pas le processus. C'est ce qui garde le
        // serveur HTTP debout quand SlimProto tombe (`background.rs` detache
        // cette tache et se contente de journaliser).
        assert!(Arc::clone(&serveur).spawn().await.is_err());
        assert!(!etat_ecoute().unwrap().ecoute);

        drop(squatteur);

        let tache = tokio::spawn(Arc::clone(&serveur).spawn());
        let etat = attendre_etat(|e| e.port == port && e.ecoute)
            .await
            .expect("la seconde tentative devait reussir une fois le port libere");
        assert!(
            etat.cause.is_none(),
            "l'etat doit oublier la cause du premier echec quand l'ecoute reprend"
        );
        tache.abort();
    }

    /// La sonde, seule, sur les deux faits qu'elle doit separer : un port tenu
    /// par un socket bien reel, et un port que personne ne tient.
    #[tokio::test]
    async fn la_sonde_separe_un_port_tenu_d_un_port_libre() {
        let boucle = vec!["127.0.0.1".to_string()];

        let occupe = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port_occupe = occupe.local_addr().unwrap().port();
        assert_eq!(
            sonder_qui_tient_le_port(port_occupe, &boucle).await,
            CausePortIndisponible::UnAutreServeurEcoute
        );
        drop(occupe);

        let libre = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port_libre = libre.local_addr().unwrap().port();
        drop(libre);
        assert_eq!(
            sonder_qui_tient_le_port(port_libre, &boucle).await,
            CausePortIndisponible::PersonneNEcoute
        );
    }

    /// Le suspect « SO_REUSEADDR manquant » du ticket, tranche par la mesure et
    /// non par elimination.
    ///
    /// Ce n'est PAS une garde du correctif : c'est la caracterisation de la
    /// plateforme. `std`/`tokio` posent deja SO_REUSEADDR sur un listener sous
    /// Unix — un TIME_WAIT ne peut donc pas expliquer les `os error 98` des
    /// journaux Linux. (Sous Windows, `std` ne le pose deliberement PAS : l'y
    /// poser laisserait un tiers detourner une ecoute vivante.)
    #[cfg(unix)]
    #[tokio::test]
    async fn le_bind_pose_deja_so_reuseaddr_sous_unix() {
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let mut valeur: libc::c_int = 0;
        let mut taille = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let code = unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &raw mut valeur as *mut libc::c_void,
                &mut taille,
            )
        };
        assert_eq!(code, 0, "getsockopt a echoue");
        assert_ne!(
            valeur, 0,
            "SO_REUSEADDR n'est PAS pose : le suspect « TIME_WAIT apres un \
             redemarrage rapide » redeviendrait plausible sous Linux"
        );
    }
}
