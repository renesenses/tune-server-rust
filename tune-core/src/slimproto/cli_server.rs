//! LMS CLI telnet bridge (port 9090).
//!
//! Exposes a telnet-style command interface compatible with Squeeze-LX
//! and other LMS controllers. Maps CLI commands to Tune's internal
//! playback, zone, and library APIs.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::PlayerRegistry;

const CLI_PORT: u16 = 9090;

/// State shared across CLI connections.
pub struct CliState {
    pub players: PlayerRegistry,
    pub server_name: String,
    pub server_version: String,
    pub local_ip: String,
}

/// Start the CLI telnet server on port 9090.
pub async fn start_cli_server(state: Arc<CliState>) {
    let port = std::env::var("TUNE_CLI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CLI_PORT);

    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, port, "lms_cli_server_bind_failed");
            return;
        }
    };

    info!(port, "lms_cli_server_started");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(peer = %peer, "lms_cli_client_connected");
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_cli_client(stream, peer, state).await {
                        debug!(peer = %peer, error = %e, "lms_cli_client_error");
                    }
                    info!(peer = %peer, "lms_cli_client_disconnected");
                });
            }
            Err(e) => {
                warn!(error = %e, "lms_cli_accept_error");
            }
        }
    }
}

async fn handle_cli_client(
    stream: TcpStream,
    _peer: SocketAddr,
    state: Arc<CliState>,
) -> Result<(), String> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        debug!(cmd = %line, "lms_cli_command_received");
        let response = handle_command(&line, &state).await;
        let out = format!("{response}\n");
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }

    Ok(())
}

async fn handle_command(line: &str, state: &Arc<CliState>) -> String {
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd = parts[0];

    // Check if first token is a player MAC address (contains ":")
    if cmd.contains(':') || cmd.contains("%3A") {
        // Player-scoped command: "<mac> <command>"
        let player_id = urldecode(cmd);
        let sub_cmd = parts.get(1).unwrap_or(&"");
        return handle_player_command(&player_id, sub_cmd, state).await;
    }

    // Global commands
    let full = line;
    match cmd {
        "login" => format!("{full} ******"),
        "listen" => format!("{full}"),
        "can" => handle_can(full),
        "player" => handle_player_query(full, state).await,
        "players" => handle_players(full, state).await,
        "serverstatus" => handle_serverstatus(full, state).await,
        "status" => handle_global_status(full, state).await,
        "pref" => handle_pref(full, state),
        "version" => format!("version {}", state.server_version),
        "connected" => "connected 1".to_string(),
        "subscribe" => format!("{full}"),
        _ => {
            debug!(cmd = full, "lms_cli_unknown_command");
            format!("{full}")
        }
    }
}

/// Handle "player count ?" and "player id/name N ?" queries.
async fn handle_player_query(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len();

    if line.contains("count") {
        return format!("player count {count}");
    }
    // "player id 0 ?" → return MAC of player at index 0
    if line.contains(" id ") {
        let mac = players
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "00:11:22:33:44:55".to_string());
        let idx = line
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("player id {idx} {mac}");
    }
    // "player name 0 ?" → return name of player at index 0
    if line.contains(" name ") {
        let name = players
            .values()
            .next()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Tune".to_string());
        let idx = line
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("player name {idx} {}", cli_encode(&name));
    }

    format!("{line}")
}

/// Handle global "status - 1 subscribe:-" command.
async fn handle_global_status(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let mac = players
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "00:11:22:33:44:55".to_string());
    let name = players
        .values()
        .next()
        .map(|p| cli_encode(&p.name))
        .unwrap_or_else(|| "Tune".to_string());
    let elapsed = players
        .values()
        .next()
        .map(|p| p.elapsed_ms / 1000)
        .unwrap_or(0);
    drop(players);

    // Return a status response that satisfies Squeeze-LX's subscription handshake
    format!(
        "{line} player_name:{name} player_connected:1 \
         player_ip:{ip}:3483 power:1 signalstrength:0 mode:play \
         time:{elapsed} duration:300 \
         playlist%20repeat:0 playlist%20shuffle:0 \
         playlist%20mode:off playlist_cur_index:0 \
         playlist_timestamp:0 playlist_tracks:1 \
         mixer%20volume:80 playerid:{mac}",
        ip = state.local_ip,
    )
}

fn handle_can(line: &str) -> String {
    // Squeeze-LX checks capabilities. Answer 0 for unimplemented features.
    // e.g. "can material-skin items ?" → "can material-skin items 0"
    let without_q = line.trim_end_matches(" ?").trim_end_matches("?");
    format!("{without_q} 0")
}

async fn handle_players(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len();

    if players.is_empty() {
        // No real players connected — return count:0.
        // Do NOT expose a virtual player or the squeezebox poller
        // will auto-create a ghost zone that steals playback.
        return format!("{line} count:0");
    }

    let mut resp = format!("{line} count:{count}");
    for (i, (mac, player)) in players.iter().enumerate() {
        let ip = player.addr.ip();
        resp.push_str(&format!(
            " playerindex:{i} playerid:{mac} uuid:tune-{mac} \
             ip:{ip}:3483 name:{name} model:squeezelite \
             modelname:Squeezelite power:1 isplaying:1 connected:1 firmware:tune",
            name = cli_encode(&player.name),
        ));
    }
    resp
}

async fn handle_serverstatus(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len().max(1);
    format!(
        "{line} lastscan:0 version:{ver} uuid:tune-server \
         info%20total%20albums:0 info%20total%20artists:0 info%20total%20songs:0 \
         player%20count:{count} other%20player%20count:0",
        ver = state.server_version,
    )
}

fn handle_pref(line: &str, state: &Arc<CliState>) -> String {
    if line.contains('?') {
        let key = line.split_whitespace().nth(1).unwrap_or("unknown");
        let value = match key {
            "httpport" => {
                let port = std::env::var("TUNE_PORT").unwrap_or_else(|_| "8888".into());
                port
            }
            "language" => "en".to_string(),
            "skin" => "Default".to_string(),
            _ => String::new(),
        };
        format!("pref {key} {value}")
    } else {
        line.to_string()
    }
}

async fn handle_player_command(player_id: &str, cmd: &str, state: &Arc<CliState>) -> String {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    let action = parts[0];
    let args = parts.get(1).unwrap_or(&"");

    let encoded_id = cli_encode_raw(player_id);

    match action {
        "status" => {
            // Squeeze-LX requests player status
            let players = state.players.lock().await;
            let elapsed = players
                .get(player_id)
                .map(|p| p.elapsed_ms / 1000)
                .unwrap_or(0);
            format!(
                "{encoded_id} status {args} \
                 player_name:Tune mode:play time:{elapsed} \
                 duration:300 playlist%20repeat:0 \
                 playlist%20shuffle:0 mixer%20volume:80 \
                 playlist_cur_index:0 playlist_tracks:1"
            )
        }
        "mixer" => format!("{encoded_id} mixer {args}"),
        "play" | "pause" | "stop" | "playlist" => {
            format!("{encoded_id} {cmd}")
        }
        "time" => format!("{encoded_id} time {args}"),
        "mode" => format!("{encoded_id} mode play"),
        "connected" => format!("{encoded_id} connected 1"),
        "signalstrength" => format!("{encoded_id} signalstrength 100"),
        "name" => {
            let players = state.players.lock().await;
            let name = players
                .get(player_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Tune".to_string());
            format!("{encoded_id} name {}", cli_encode(&name))
        }
        "power" => format!("{encoded_id} power 1"),
        _ => {
            debug!(player = player_id, cmd, "lms_cli_unknown_player_command");
            format!("{encoded_id} {cmd}")
        }
    }
}

/// Encode a value for CLI response — percent-encode spaces but keep colons raw.
fn cli_encode(s: &str) -> String {
    s.replace(' ', "%20").replace('\n', "")
}

/// Encode a player ID for response — keep colons raw (critical for Squeeze-LX).
fn cli_encode_raw(s: &str) -> String {
    s.replace(' ', "%20")
}

/// Decode a percent-encoded value.
fn urldecode(s: &str) -> String {
    urlencoding::decode(s)
        .unwrap_or_else(|_| s.into())
        .to_string()
}
