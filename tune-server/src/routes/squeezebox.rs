use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::squeezebox::LMS_CLI_PORT;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(squeezebox_status))
        .route("/players", get(list_players))
        .route("/discover", post(discover_players))
        .route("/players/{id}/play", post(play_player))
        .route("/players/{id}/pause", post(pause_player))
        .route("/players/{id}/volume", post(set_player_volume))
        .route("/players/{id}/power", post(power_player))
}

/// Parse the squeezebox_host setting into (host, port).
/// Default CLI port is 9090.
fn parse_lms_host(state: &AppState) -> (String, u16) {
    let settings = SettingsRepo::new(state.db.clone());
    let raw = settings
        .get("squeezebox_host")
        .ok()
        .flatten()
        .unwrap_or_else(|| "localhost".into());

    if raw.contains(':') {
        let parts: Vec<&str> = raw.splitn(2, ':').collect();
        let port = parts[1].parse::<u16>().unwrap_or(LMS_CLI_PORT);
        (parts[0].to_string(), port)
    } else {
        (raw, LMS_CLI_PORT)
    }
}

/// Send a raw CLI command to LMS via TCP and return the response line.
fn lms_cli_command(host: &str, port: u16, cmd: &str) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .map_err(|e| format!("invalid LMS address {addr}: {e}"))?,
        Duration::from_secs(5),
    )
    .map_err(|e| {
        format!(
            "Impossible de se connecter au serveur Squeezebox (LMS) sur {addr}: {e}. Verifiez que Logitech Media Server est demarre."
        )
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set write timeout: {e}"))?;

    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;
    let line = format!("{cmd}\n");
    writer
        .write_all(line.as_bytes())
        .map_err(|e| format!("LMS CLI write: {e}"))?;
    writer.flush().map_err(|e| format!("LMS CLI flush: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|e| format!("LMS CLI read: {e}"))?;

    let decoded = urlencoding::decode(response.trim())
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| response.trim().to_string());

    Ok(decoded)
}

/// Send a player-scoped CLI command.
fn lms_player_command(host: &str, port: u16, player_id: &str, cmd: &str) -> Result<String, String> {
    let encoded_mac = urlencoding::encode(player_id);
    lms_cli_command(host, port, &format!("{encoded_mac} {cmd}"))
}

async fn squeezebox_status(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_cli_command(&host, port, "serverstatus 0 100") {
        Ok(resp) => Json(json!({"status": "ok", "response": resp})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn list_players(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match list_players_cli(&host, port) {
        Ok(players) => Json(json!(players)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Discover players via CLI commands: `player count ?`, then `player id/name {i} ?`
fn list_players_cli(host: &str, port: u16) -> Result<Vec<Value>, String> {
    let count_resp = lms_cli_command(host, port, "player count ?")?;
    // Response: "player count 3"
    let count: usize = count_resp
        .rsplit(' ')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut players = Vec::new();
    for i in 0..count {
        let id_resp = lms_cli_command(host, port, &format!("player id {i} ?"))?;
        let name_resp = lms_cli_command(host, port, &format!("player name {i} ?"))?;

        // Response: "player id 0 00:04:20:ab:cd:ef"
        let player_id = id_resp.rsplit(' ').next().unwrap_or("").to_string();
        // Response: "player name 0 Kitchen"
        let player_name = name_resp
            .rsplitn(2, &format!("player name {i} "))
            .next()
            .unwrap_or("Squeezebox")
            .to_string();
        // Better extraction: everything after the last known prefix
        let player_name = if let Some(pos) = name_resp.find(&format!("player name {i} ")) {
            let start = pos + format!("player name {i} ").len();
            name_resp[start..].to_string()
        } else {
            player_name
        };

        if !player_id.is_empty() {
            players.push(json!({
                "playerid": player_id,
                "name": player_name,
            }));
        }
    }
    Ok(players)
}

async fn play_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, "play") {
        Ok(_) => Json(json!({"status": "playing"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn pause_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, "pause") {
        Ok(_) => Json(json!({"status": "paused"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: u8,
}

#[derive(Deserialize)]
struct PowerBody {
    #[serde(default = "default_power_on")]
    state: u8,
}

fn default_power_on() -> u8 {
    1
}

async fn set_player_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, &format!("mixer volume {}", body.volume)) {
        Ok(_) => Json(json!({"volume": body.volume})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn power_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PowerBody>,
) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    let label = if body.state == 1 { "on" } else { "off" };
    match lms_player_command(&host, port, &id, &format!("power {}", body.state)) {
        Ok(_) => Json(json!({"power": label})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn discover_players(State(state): State<AppState>) -> impl IntoResponse {
    match discover_and_register(&state).await {
        Ok(registered) => Json(json!({
            "discovered": registered.len(),
            "players": registered,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Query LMS for connected players via CLI and register them as Squeezebox outputs + auto-create zones.
/// Called at startup (when squeezebox_enabled=true) and via POST /squeezebox/discover.
pub async fn discover_and_register(state: &AppState) -> Result<Vec<Value>, String> {
    let (lms_host_str, lms_port) = parse_lms_host(state);

    let players = list_players_cli(&lms_host_str, lms_port)?;

    if players.is_empty() {
        tracing::info!(host = %lms_host_str, port = lms_port, "squeezebox_discover: no players found on LMS");
        return Ok(vec![]);
    }

    let mut registered = Vec::new();
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::new(state.db.clone());
    let existing_zones = zone_repo.list().unwrap_or_default();

    for player in &players {
        let player_id = match player.get("playerid").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let player_name = player
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Squeezebox")
            .to_string();
        let device_id = format!("squeezebox-{player_id}");

        // Register output using CLI port
        let output = tune_core::outputs::squeezebox::SqueezeboxOutput::new(
            player_name.clone(),
            device_id.clone(),
            lms_host_str.clone(),
            lms_port,
        );
        {
            let mut reg = state.outputs.lock().await;
            reg.register(Box::new(output));
        }
        tracing::info!(name = %player_name, id = %device_id, lms_host = %lms_host_str, lms_port, "squeezebox_output_registered");

        // Auto-create zone if not already present
        let already_by_device = existing_zones
            .iter()
            .any(|z| z.output_device_id.as_deref() == Some(&device_id));
        if already_by_device {
            let _ = zone_repo.set_online_by_device(&device_id, true);
            tracing::info!(name = %player_name, id = %device_id, "squeezebox_zone_reconnected");
        } else {
            let name_taken = existing_zones.iter().any(|z| z.name == player_name);
            if !name_taken {
                if let Ok(zid) =
                    zone_repo.create(&player_name, Some("squeezebox"), Some(&device_id))
                {
                    tracing::info!(name = %player_name, zone_id = zid, "squeezebox_zone_auto_created");
                }
            }
        }

        registered.push(json!({
            "id": device_id,
            "name": player_name,
            "playerid": player_id,
        }));
    }

    Ok(registered)
}
