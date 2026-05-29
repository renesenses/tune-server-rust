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
async fn lms_request(host: &str, player: &str, cmd: Vec<Value>) -> Result<Value, String> {
    let url = lms_url(host);
    let body = json!({
        "id": 1,
        "method": "slim.request",
        "params": [player, cmd],
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("http client error: {e}"))?;
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
    match lms_request(&host, "", vec![json!("serverstatus"), json!(0), json!(100)]).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn list_players(State(state): State<AppState>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&host, "", vec![json!("players"), json!(0), json!(100)]).await {
        Ok(result) => {
            let players = result.get("players_loop").cloned().unwrap_or(json!([]));
            Json(players).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn play_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&host, &id, vec![json!("play")]).await {
        Ok(result) => Json(json!({"status": "playing", "result": result})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn pause_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let host = lms_host(&state);
    match lms_request(&host, &id, vec![json!("pause")]).await {
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
