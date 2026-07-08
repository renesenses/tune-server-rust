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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

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
        /// Remaining bytes may contain device name (UTF-8, variable length).
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
            // Bytes 8..9 are typically the number of wlan channels, bytes 10+
            // may contain the device name as UTF-8.
            let name = if payload.len() > 10 {
                String::from_utf8_lossy(&payload[10..])
                    .trim_end_matches('\0')
                    .to_string()
            } else {
                String::new()
            };

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
        }
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
                event_bus,
                outputs,
                server_ip,
                command_channels: Arc::new(Mutex::new(HashMap::new())),
            })),
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
    pub async fn spawn(self: Arc<Self>) -> Result<(), String> {
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("slimproto bind {addr}: {e}"))?;

        info!(port = self.port, "slimproto_server_started");

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
                        }
                    }
                    Ok(ClientMessage::Bye) => {
                        info!(mac = %mac_for_reader, "slimproto_bye_received");
                        break Ok(());
                    }
                    Ok(ClientMessage::Dsco { reason }) => {
                        info!(mac = %mac_for_reader, reason, "slimproto_dsco_received");
                        // Player disconnected from the audio stream — not from us.
                        // Stay connected and keep heartbeating.
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
        let player_name = {
            let reg = self.players.lock().await;
            match reg.get(mac_str) {
                Some(p) => p.name.clone(),
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
                    state.event_bus.emit_typed(
                        crate::event_types::EventType::ZoneCreated,
                        serde_json::json!({
                            "zone_id": zone_id,
                            "name": player_name.clone(),
                            "device_id": device_id.clone(),
                            "type": "slimproto",
                        }),
                    );
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
        let output = crate::outputs::slimproto::SlimProtoOutput::new(
            player_name,
            device_id,
            mac_str.to_string(),
            Arc::clone(&self.players),
            Arc::clone(&state.command_channels),
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

    let len = reader
        .read_u16()
        .await
        .map_err(|e| format!("read length: {e}"))? as usize;

    if len < 4 {
        return Err(format!("message too short: len={len}"));
    }

    let mut tag = [0u8; 4];
    reader
        .read_exact(&mut tag)
        .await
        .map_err(|e| format!("read tag: {e}"))?;

    let payload_len = len - 4;
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
}
