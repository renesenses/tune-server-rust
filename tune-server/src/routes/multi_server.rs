use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteServer {
    name: String,
    server_id: String,
    bridge_token: String,
    #[serde(default)]
    last_seen: Option<String>,
    #[serde(default = "default_status")]
    status: String,
}

fn default_status() -> String {
    "unknown".to_string()
}

#[derive(Deserialize)]
struct AddServerRequest {
    name: String,
    server_id: String,
    bridge_token: String,
}

#[derive(Deserialize)]
struct ProxyRequest {
    method: String,
    path: String,
    body: Option<Value>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const SETTING_KEY: &str = "multi_server_list";

fn load_servers(settings: &SettingsRepo) -> Vec<RemoteServer> {
    settings
        .get(SETTING_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<RemoteServer>>(&s).ok())
        .unwrap_or_default()
}

fn save_servers(settings: &SettingsRepo, servers: &[RemoteServer]) -> Result<(), String> {
    let json = serde_json::to_string(servers).map_err(|e| e.to_string())?;
    settings.set(SETTING_KEY, &json)
}

async fn proxy_to_remote(
    http_client: &reqwest::Client,
    server_id: &str,
    bridge_token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!(
        "https://bridge.mozaiklabs.fr/api/relay/{server_id}/{}",
        path.trim_start_matches('/')
    );
    let req = match method.to_uppercase().as_str() {
        "GET" => http_client.get(&url),
        "POST" => {
            let r = http_client.post(&url);
            if let Some(b) = body { r.json(b) } else { r }
        }
        "PUT" => {
            let r = http_client.put(&url);
            if let Some(b) = body { r.json(b) } else { r }
        }
        "DELETE" => http_client.delete(&url),
        other => return Err(format!("unsupported method: {other}")),
    };
    let resp = req
        .header("Authorization", format!("BridgeToken {bridge_token}"))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

fn find_server<'a>(servers: &'a [RemoteServer], server_id: &str) -> Option<&'a RemoteServer> {
    servers.iter().find(|s| s.server_id == server_id)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/servers", get(list_servers).post(add_server))
        .route("/servers/{server_id}", delete(remove_server))
        .route("/servers/{server_id}/zones", get(remote_zones))
        .route(
            "/servers/{server_id}/library/albums",
            get(remote_library_albums),
        )
        .route("/servers/{server_id}/proxy", post(generic_proxy))
        .route("/unified/zones", get(unified_zones))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /multi-server/servers — list registered remote servers.
async fn list_servers(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let servers = load_servers(&settings);

    Json(json!({
        "servers": servers,
        "count": servers.len(),
    }))
    .into_response()
}

/// POST /multi-server/servers — register a new remote server.
async fn add_server(
    State(state): State<AppState>,
    Json(body): Json<AddServerRequest>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    if body.name.is_empty() || body.server_id.is_empty() || body.bridge_token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name, server_id and bridge_token are required"})),
        )
            .into_response();
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut servers = load_servers(&settings);

    // Reject duplicates
    if servers.iter().any(|s| s.server_id == body.server_id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "server_id already registered"})),
        )
            .into_response();
    }

    // Validate by pinging the relay
    let ping_result = proxy_to_remote(
        &state.http_client,
        &body.server_id,
        &body.bridge_token,
        "GET",
        "/api/v1/system/status",
        None,
    )
    .await;

    let status = if ping_result.is_ok() {
        "online".to_string()
    } else {
        warn!(
            server_id = %body.server_id,
            error = ?ping_result.err(),
            "multi_server_ping_failed_on_add"
        );
        "offline".to_string()
    };

    let now = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("{secs}")
    };

    let server = RemoteServer {
        name: body.name.clone(),
        server_id: body.server_id.clone(),
        bridge_token: body.bridge_token.clone(),
        last_seen: Some(now),
        status,
    };

    servers.push(server.clone());
    if let Err(e) = save_servers(&settings, &servers) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to save: {e}")})),
        )
            .into_response();
    }

    info!(
        name = %body.name,
        server_id = %body.server_id,
        status = %server.status,
        "multi_server_added"
    );

    (StatusCode::CREATED, Json(json!({"server": server}))).into_response()
}

/// DELETE /multi-server/servers/{server_id} — remove a remote server.
async fn remove_server(
    Path(server_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut servers = load_servers(&settings);

    let before = servers.len();
    servers.retain(|s| s.server_id != server_id);

    if servers.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "server_id not found"})),
        )
            .into_response();
    }

    if let Err(e) = save_servers(&settings, &servers) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to save: {e}")})),
        )
            .into_response();
    }

    info!(server_id = %server_id, "multi_server_removed");
    Json(json!({"removed": server_id})).into_response()
}

/// GET /multi-server/servers/{server_id}/zones — proxy zones from a remote server.
async fn remote_zones(
    Path(server_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let servers = load_servers(&settings);

    let Some(server) = find_server(&servers, &server_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "server_id not found"})),
        )
            .into_response();
    };

    match proxy_to_remote(
        &state.http_client,
        &server.server_id,
        &server.bridge_token,
        "GET",
        "/api/v1/zones",
        None,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            warn!(server_id = %server_id, error = %e, "multi_server_zones_proxy_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("relay error: {e}")})),
            )
                .into_response()
        }
    }
}

/// GET /multi-server/servers/{server_id}/library/albums — proxy albums from a remote server.
async fn remote_library_albums(
    Path(server_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let servers = load_servers(&settings);

    let Some(server) = find_server(&servers, &server_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "server_id not found"})),
        )
            .into_response();
    };

    match proxy_to_remote(
        &state.http_client,
        &server.server_id,
        &server.bridge_token,
        "GET",
        "/api/v1/library/albums",
        None,
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            warn!(server_id = %server_id, error = %e, "multi_server_albums_proxy_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("relay error: {e}")})),
            )
                .into_response()
        }
    }
}

/// POST /multi-server/servers/{server_id}/proxy — generic proxy to any endpoint.
async fn generic_proxy(
    Path(server_id): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<ProxyRequest>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let servers = load_servers(&settings);

    let Some(server) = find_server(&servers, &server_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "server_id not found"})),
        )
            .into_response();
    };

    match proxy_to_remote(
        &state.http_client,
        &server.server_id,
        &server.bridge_token,
        &body.method,
        &body.path,
        body.body.as_ref(),
    )
    .await
    {
        Ok(data) => Json(data).into_response(),
        Err(e) => {
            warn!(
                server_id = %server_id,
                method = %body.method,
                path = %body.path,
                error = %e,
                "multi_server_generic_proxy_failed"
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("relay error: {e}")})),
            )
                .into_response()
        }
    }
}

/// GET /multi-server/unified/zones — aggregate zones from all servers + local.
async fn unified_zones(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::MultiServer).await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let servers = load_servers(&settings);

    // Collect local zones
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let local_zones = zone_repo.list().unwrap_or_default();

    let server_name = settings
        .get("server_name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Local".to_string());

    let mut all_zones: Vec<Value> = local_zones
        .iter()
        .map(|z| {
            let mut v = serde_json::to_value(z).unwrap_or(json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("server_name".into(), json!(server_name));
                obj.insert("server_id".into(), json!("local"));
                obj.insert("remote".into(), json!(false));
            }
            v
        })
        .collect();

    // Fetch remote zones in parallel
    let mut handles = Vec::new();
    for server in &servers {
        let http = state.http_client.clone();
        let sid = server.server_id.clone();
        let token = server.bridge_token.clone();
        let name = server.name.clone();
        handles.push(tokio::spawn(async move {
            let result = proxy_to_remote(&http, &sid, &token, "GET", "/api/v1/zones", None).await;
            (sid, name, result)
        }));
    }

    for handle in handles {
        if let Ok((sid, name, Ok(data))) = handle.await {
            // The zones endpoint may return {"zones": [...]} or a bare array
            let zones_arr = data
                .get("zones")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| data.as_array().cloned())
                .unwrap_or_default();

            for mut zone in zones_arr {
                if let Some(obj) = zone.as_object_mut() {
                    obj.insert("server_name".into(), json!(name));
                    obj.insert("server_id".into(), json!(sid));
                    obj.insert("remote".into(), json!(true));
                }
                all_zones.push(zone);
            }
        }
    }

    Json(json!({
        "zones": all_zones,
        "count": all_zones.len(),
        "servers": servers.len() + 1, // +1 for local
    }))
    .into_response()
}
