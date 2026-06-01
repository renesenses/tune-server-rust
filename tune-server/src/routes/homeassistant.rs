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
        .route("/status", get(ha_status))
        .route("/config", get(ha_config).post(set_ha_config))
        .route("/entities", get(ha_entities))
        .route("/entities/{entity_id}/state", get(ha_entity_state))
        .route("/entities/{entity_id}/call", post(ha_call_service))
        .route("/media-players", get(ha_media_players))
        .route("/automations", get(ha_automations))
        .route("/automations/trigger", post(ha_trigger_automation))
}

fn ha_settings(state: &AppState) -> (Option<String>, Option<String>) {
    let settings = SettingsRepo::new(state.db.clone());
    let url = settings.get("ha_url").ok().flatten();
    let token = settings.get("ha_token").ok().flatten();
    (url, token)
}

async fn ha_status(State(state): State<AppState>) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return Json(json!({
            "configured": false,
            "connected": false,
            "message": "Home Assistant not configured. Set ha_url and ha_token.",
        }))
        .into_response();
    };
    match state
        .http_client
        .get(format!("{url}/api/"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(json!({
                "configured": true,
                "connected": true,
                "message": body.get("message").cloned().unwrap_or(json!("OK")),
            }))
            .into_response()
        }
        Ok(resp) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "configured": true,
                "connected": false,
                "error": format!("HA returned status {}", resp.status()),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "configured": true,
                "connected": false,
                "error": format!("Connection failed: {e}"),
            })),
        )
            .into_response(),
    }
}

async fn ha_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let url = settings.get("ha_url").ok().flatten().unwrap_or_default();
    let has_token = settings.get("ha_token").ok().flatten().is_some();
    Json(json!({
        "ha_url": url,
        "ha_token_set": has_token,
    }))
}

#[derive(Deserialize)]
struct HaConfigBody {
    ha_url: Option<String>,
    ha_token: Option<String>,
}

async fn set_ha_config(
    State(state): State<AppState>,
    Json(body): Json<HaConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    if let Some(url) = &body.ha_url {
        settings.set("ha_url", url.trim_end_matches('/')).ok();
    }
    if let Some(token) = &body.ha_token {
        settings.set("ha_token", token).ok();
    }
    Json(json!({"saved": true}))
}

async fn ha_entities(State(state): State<AppState>) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    match state
        .http_client
        .get(format!("{url}/api/states"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!([]));
            Json(body).into_response()
        }
        Ok(resp) => {
            let status = resp.status();
            let err: Value = resp.json().await.unwrap_or(json!({}));
            let msg = format!("HA {status}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": msg, "detail": err})),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn ha_entity_state(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    match state
        .http_client
        .get(format!("{url}/api/states/{entity_id}"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) if resp.status().as_u16() == 404 => {
            let msg = format!("Entity {entity_id} not found");
            (StatusCode::NOT_FOUND, Json(json!({"error": msg}))).into_response()
        }
        Ok(resp) => {
            let msg = format!("HA returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct CallServiceBody {
    domain: String,
    service: String,
    service_data: Option<Value>,
}

async fn ha_call_service(
    State(state): State<AppState>,
    Path(entity_id): Path<String>,
    Json(body): Json<CallServiceBody>,
) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    let mut payload = body.service_data.unwrap_or(json!({}));
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("entity_id".into(), json!(entity_id));
    }
    let api_url = format!("{url}/api/services/{}/{}", body.domain, body.service);
    match state
        .http_client
        .post(&api_url)
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({"success": true, "result": result})).into_response()
        }
        Ok(resp) => {
            let status = resp.status();
            let err: Value = resp.json().await.unwrap_or(json!({}));
            let msg = format!("HA returned {status}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": msg, "detail": err})),
            )
                .into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn ha_media_players(State(state): State<AppState>) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    match state
        .http_client
        .get(format!("{url}/api/states"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!([]));
            let players: Vec<&Value> = body
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter(|e| {
                            e.get("entity_id")
                                .and_then(|id| id.as_str())
                                .map(|id| id.starts_with("media_player."))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Json(json!(players)).into_response()
        }
        Ok(resp) => {
            let msg = format!("HA returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn ha_automations(State(state): State<AppState>) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    match state
        .http_client
        .get(format!("{url}/api/states"))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!([]));
            let automations: Vec<&Value> = body
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter(|e| {
                            e.get("entity_id")
                                .and_then(|id| id.as_str())
                                .map(|id| id.starts_with("automation."))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Json(json!(automations)).into_response()
        }
        Ok(resp) => {
            let msg = format!("HA returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct TriggerAutomationBody {
    entity_id: String,
}

async fn ha_trigger_automation(
    State(state): State<AppState>,
    Json(body): Json<TriggerAutomationBody>,
) -> impl IntoResponse {
    let (url, token) = ha_settings(&state);
    let (Some(url), Some(token)) = (url, token) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Home Assistant not configured"})),
        )
            .into_response();
    };
    let payload = json!({"entity_id": body.entity_id});
    match state
        .http_client
        .post(format!("{url}/api/services/automation/trigger"))
        .bearer_auth(&token)
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({"success": true, "result": result})).into_response()
        }
        Ok(resp) => {
            let msg = format!("HA returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}
