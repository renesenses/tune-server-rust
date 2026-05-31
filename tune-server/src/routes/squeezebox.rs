use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(squeezebox_status))
        .route("/players", get(list_players))
        .route("/discover", post(discover_players))
        .route("/players/{id}/play", post(play_player))
        .route("/players/{id}/pause", post(pause_player))
        .route("/players/{id}/volume", post(set_player_volume))
}

fn lms_host(state: &AppState) -> String {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("squeezebox_host")
        .ok()
        .flatten()
        .unwrap_or_else(|| "localhost".into())
}

fn lms_url(host: &str) -> String {
    if host.contains(':') {
        format!("http://{host}/jsonrpc.js")
    } else {
        format!("http://{host}:9000/jsonrpc.js")
    }
}

/// Send a JSON-RPC request to LMS. LMS uses a "slim" protocol:
/// `{ "id": 1, "method": "slim.request", "params": [player_id, [command, ...]] }`
async fn lms_request(client: &reqwest::Client, host: &str, player: &str, cmd: Vec<Value>) -> Result<Value, String> {
    let url = lms_url(host);
    let body = json!({
        "id": 1,
        "method": "slim.request",
        "params": [player, cmd],
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                format!("Impossible de se connecter au serveur Squeezebox (LMS) sur {host}. Vérifiez que Logitech Media Server est démarré.")
            } else if e.is_timeout() {
                format!("Le serveur Squeezebox (LMS) sur {host} ne répond pas (timeout).")
            } else {
                format!("LMS request failed: {e}")
            }
        })?;
    let text = resp
        .text()
        .await
        .map_err(|e| format!("LMS read error: {e}"))?;
    if text.is_empty() {
        return Err(format!(
            "Le serveur sur {host} a renvoyé une réponse vide. Vérifiez qu'il s'agit bien d'un serveur Squeezebox/LMS."
        ));
    }
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("Réponse invalide du serveur LMS: {e}"))?;
    Ok(json.get("result").cloned().unwrap_or(Value::Null))
}

async fn squeezebox_status(State(state): State<AppState>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&state.http_client, &host, "", vec![json!("serverstatus"), json!(0), json!(100)]).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn list_players(State(state): State<AppState>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&state.http_client, &host, "", vec![json!("players"), json!(0), json!(100)]).await {
        Ok(result) => {
            let players = result.get("players_loop").cloned().unwrap_or(json!([]));
            Json(players).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn play_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&state.http_client, &host, &id, vec![json!("play")]).await {
        Ok(result) => Json(json!({"status": "playing", "result": result})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn pause_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&state.http_client, &host, &id, vec![json!("pause")]).await {
        Ok(result) => Json(json!({"status": "paused", "result": result})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: u8,
}

async fn set_player_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(
        &state.http_client,
        &host,
        &id,
        vec![json!("mixer"), json!("volume"), json!(body.volume)],
    )
    .await
    {
        Ok(result) => Json(json!({"volume": body.volume, "result": result})).into_response(),
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

/// Query LMS for connected players and register them as Squeezebox outputs + auto-create zones.
/// Called at startup (when squeezebox_enabled=true) and via POST /squeezebox/discover.
pub async fn discover_and_register(state: &AppState) -> Result<Vec<Value>, String> {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db.clone());
    let host = settings
        .get("squeezebox_host")
        .ok()
        .flatten()
        .unwrap_or_else(|| "localhost".into());

    // Parse host:port from the setting
    let (lms_host_str, lms_port) = if host.contains(':') {
        let parts: Vec<&str> = host.splitn(2, ':').collect();
        let port = parts[1].parse::<u16>().unwrap_or(9000);
        (parts[0].to_string(), port)
    } else {
        (host.clone(), 9000u16)
    };

    let result = lms_request(&state.http_client, &host, "", vec![json!("players"), json!(0), json!(100)]).await?;
    let players = result
        .get("players_loop")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if players.is_empty() {
        tracing::info!(host = %host, "squeezebox_discover: no players found on LMS");
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

        // Register output
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
        tracing::info!(name = %player_name, id = %device_id, lms = %host, "squeezebox_output_registered");

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
            "model": player.get("modelname"),
            "connected": player.get("connected"),
        }));
    }

    Ok(registered)
}
