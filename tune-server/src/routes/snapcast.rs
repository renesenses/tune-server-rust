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
        .route("/status", get(snapcast_status))
        .route("/clients", get(list_clients))
        .route("/clients/{id}/volume", post(set_client_volume))
        .route("/clients/{id}/mute", post(mute_client))
        .route("/groups", get(list_groups))
        .route("/groups/{id}/stream", post(set_group_stream))
}

fn snapcast_host(state: &AppState) -> String {
    let settings = SettingsRepo::new(state.db.clone());
    settings
        .get("snapcast_host")
        .ok()
        .flatten()
        .unwrap_or_else(|| "localhost:1705".into())
}

async fn jsonrpc_call(client: &reqwest::Client, host: &str, method: &str, params: Value) -> Result<Value, String> {
    let url = format!("http://{host}/jsonrpc");
    let body = json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("snapcast request failed: {e}"))?;
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("snapcast response parse error: {e}"))?;
    if let Some(err) = json.get("error") {
        return Err(format!("snapcast error: {err}"));
    }
    Ok(json.get("result").cloned().unwrap_or(Value::Null))
}

async fn snapcast_status(State(state): State<AppState>) -> impl IntoResponse {
    let host = snapcast_host(&state);
    match jsonrpc_call(&state.http_client, &host, "Server.GetStatus", json!({})).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn list_clients(State(state): State<AppState>) -> impl IntoResponse {
    let host = snapcast_host(&state);
    match jsonrpc_call(&state.http_client, &host, "Server.GetStatus", json!({})).await {
        Ok(result) => {
            let clients = result
                .pointer("/server/groups")
                .and_then(|groups| groups.as_array())
                .map(|groups| {
                    groups
                        .iter()
                        .flat_map(|g| {
                            g.get("clients")
                                .and_then(|c| c.as_array())
                                .cloned()
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Json(json!(clients)).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: u8,
}

async fn set_client_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let host = snapcast_host(&state);
    let params = json!({
        "id": id,
        "volume": { "percent": body.volume, "muted": false },
    });
    match jsonrpc_call(&state.http_client, &host, "Client.SetVolume", params).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct MuteBody {
    muted: bool,
}

async fn mute_client(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MuteBody>,
) -> impl IntoResponse {
    let host = snapcast_host(&state);
    let params = json!({
        "id": id,
        "volume": { "muted": body.muted },
    });
    match jsonrpc_call(&state.http_client, &host, "Client.SetVolume", params).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn list_groups(State(state): State<AppState>) -> impl IntoResponse {
    let host = snapcast_host(&state);
    match jsonrpc_call(&state.http_client, &host, "Server.GetStatus", json!({})).await {
        Ok(result) => {
            let groups = result
                .pointer("/server/groups")
                .cloned()
                .unwrap_or(json!([]));
            Json(groups).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct StreamBody {
    stream_id: String,
}

async fn set_group_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<StreamBody>,
) -> impl IntoResponse {
    let host = snapcast_host(&state);
    let params = json!({
        "id": id,
        "stream_id": body.stream_id,
    });
    match jsonrpc_call(&state.http_client, &host, "Group.SetStream", params).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}
