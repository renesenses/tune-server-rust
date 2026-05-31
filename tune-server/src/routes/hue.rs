use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(hue_status))
        .route("/config", get(hue_config).post(set_hue_config))
        .route("/lights", get(hue_lights))
        .route("/lights/{id}", get(hue_light_state).put(set_hue_light))
        .route("/groups", get(hue_groups))
        .route("/groups/{id}", put(set_hue_group))
        .route("/scenes", get(hue_scenes))
        .route("/scenes/{id}/activate", post(activate_hue_scene))
        .route("/sync", post(sync_to_music))
}

fn hue_settings(state: &AppState) -> (Option<String>, Option<String>) {
    let settings = SettingsRepo::new(state.db.clone());
    let bridge_ip = settings.get("hue_bridge_ip").ok().flatten();
    let username = settings.get("hue_username").ok().flatten();
    (bridge_ip, username)
}

fn hue_base_url(bridge_ip: &str, username: &str) -> String {
    format!("http://{bridge_ip}/api/{username}")
}


async fn hue_status(State(state): State<AppState>) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let configured = bridge_ip.is_some() && username.is_some();
    if !configured {
        return Json(json!({
            "configured": false,
            "connected": false,
            "message": "Hue Bridge not configured. Set hue_bridge_ip and hue_username.",
        }))
        .into_response();
    }
    let base = hue_base_url(&bridge_ip.unwrap(), &username.unwrap());
    let client = &state.http_client;
    match client.get(format!("{base}/config")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(json!({
                "configured": true,
                "connected": true,
                "bridge_name": body.get("name"),
                "api_version": body.get("apiversion"),
                "model_id": body.get("modelid"),
            }))
            .into_response()
        }
        Ok(_) => Json(json!({
            "configured": true,
            "connected": false,
            "message": "Bridge returned an error",
        }))
        .into_response(),
        Err(e) => Json(json!({
            "configured": true,
            "connected": false,
            "message": format!("Connection failed: {e}"),
        }))
        .into_response(),
    }
}

async fn hue_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let bridge_ip = settings
        .get("hue_bridge_ip")
        .ok()
        .flatten()
        .unwrap_or_default();
    let has_username = settings.get("hue_username").ok().flatten().is_some();
    Json(json!({
        "hue_bridge_ip": bridge_ip,
        "hue_username_set": has_username,
    }))
}

#[derive(Deserialize)]
struct HueConfigBody {
    hue_bridge_ip: Option<String>,
    hue_username: Option<String>,
}

async fn set_hue_config(
    State(state): State<AppState>,
    Json(body): Json<HueConfigBody>,
) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    if let Some(ip) = &body.hue_bridge_ip {
        settings.set("hue_bridge_ip", ip).ok();
    }
    if let Some(user) = &body.hue_username {
        settings.set("hue_username", user).ok();
    }
    Json(json!({"saved": true}))
}

async fn hue_lights(State(state): State<AppState>) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client.get(format!("{base}/lights")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn hue_light_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client.get(format!("{base}/lights/{id}")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn set_hue_light(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client
        .put(format!("{base}/lights/{id}/state"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({"success": true, "result": result})).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn hue_groups(State(state): State<AppState>) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client.get(format!("{base}/groups")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn set_hue_group(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client
        .put(format!("{base}/groups/{id}/action"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({"success": true, "result": result})).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn hue_scenes(State(state): State<AppState>) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    match client.get(format!("{base}/scenes")).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: Value = resp.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

async fn activate_hue_scene(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;
    // Activate scene by setting it on group 0 (all lights)
    let payload = json!({"scene": id});
    match client
        .put(format!("{base}/groups/0/action"))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({"success": true, "scene_id": id, "result": result})).into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct SyncBody {
    /// RGB hex color to set (e.g. "#FF6600"), or "auto" to extract from current album art.
    color: Option<String>,
    brightness: Option<u8>,
    group_id: Option<String>,
}

async fn sync_to_music(
    State(state): State<AppState>,
    Json(body): Json<SyncBody>,
) -> impl IntoResponse {
    let (bridge_ip, username) = hue_settings(&state);
    let (Some(bridge_ip), Some(username)) = (bridge_ip, username) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Hue Bridge not configured"})),
        )
            .into_response();
    };
    let base = hue_base_url(&bridge_ip, &username);
    let client = &state.http_client;

    // Convert hex color to Hue xy color space (simplified approximation)
    let (x, y) = if let Some(hex) = &body.color {
        hex_to_xy(hex)
    } else {
        // Default warm white
        (0.4573, 0.41)
    };

    let bri = body.brightness.unwrap_or(200);
    let group_id = body.group_id.as_deref().unwrap_or("0");

    let payload = json!({
        "on": true,
        "bri": bri,
        "xy": [x, y],
        "transitiontime": 10, // 1 second transition
    });

    match client
        .put(format!("{base}/groups/{group_id}/action"))
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let result: Value = resp.json().await.unwrap_or(json!([]));
            Json(json!({
                "success": true,
                "color_xy": [x, y],
                "brightness": bri,
                "group_id": group_id,
                "result": result,
            }))
            .into_response()
        }
        Ok(resp) => {
            let msg = format!("Hue returned {}", resp.status());
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => {
            let msg = format!("Request failed: {e}");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response()
        }
    }
}

/// Convert hex color string to CIE xy coordinates (simplified sRGB to xy).
fn hex_to_xy(hex: &str) -> (f64, f64) {
    let hex = hex.trim_start_matches('#');
    if hex.len() < 6 {
        return (0.4573, 0.41); // warm white fallback
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255) as f64 / 255.0;
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255) as f64 / 255.0;
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255) as f64 / 255.0;

    // Apply gamma correction
    let r = if r > 0.04045 {
        ((r + 0.055) / 1.055).powf(2.4)
    } else {
        r / 12.92
    };
    let g = if g > 0.04045 {
        ((g + 0.055) / 1.055).powf(2.4)
    } else {
        g / 12.92
    };
    let b = if b > 0.04045 {
        ((b + 0.055) / 1.055).powf(2.4)
    } else {
        b / 12.92
    };

    // Wide RGB D65 conversion
    let x_val = r * 0.664511 + g * 0.154324 + b * 0.162028;
    let y_val = r * 0.283881 + g * 0.668433 + b * 0.047685;
    let z_val = r * 0.000088 + g * 0.072310 + b * 0.986039;

    let sum = x_val + y_val + z_val;
    if sum == 0.0 {
        return (0.4573, 0.41);
    }
    (x_val / sum, y_val / sum)
}
