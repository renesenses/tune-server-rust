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
// Constants
// ---------------------------------------------------------------------------

const SETTINGS_KEY_API_KEYS: &str = "developer_api_keys";
const SETTINGS_KEY_WEBHOOKS: &str = "developer_webhooks";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub scopes: Vec<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
struct CreateApiKeyRequest {
    name: String,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct CreateWebhookRequest {
    url: String,
    events: Vec<String>,
}

// ---------------------------------------------------------------------------
// Valid scopes & events
// ---------------------------------------------------------------------------

const VALID_SCOPES: &[&str] = &["read", "control", "write"];
const VALID_EVENTS: &[&str] = &[
    "track.started",
    "track.ended",
    "zone.changed",
    "volume.changed",
];

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{key_id}", delete(revoke_api_key))
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route("/webhooks/{id}", delete(delete_webhook))
        .route("/webhooks/test", post(test_webhooks))
}

// ---------------------------------------------------------------------------
// Helpers — settings persistence
// ---------------------------------------------------------------------------

fn load_api_keys(settings: &SettingsRepo) -> Vec<DevApiKey> {
    settings
        .get(SETTINGS_KEY_API_KEYS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_api_keys(settings: &SettingsRepo, keys: &[DevApiKey]) {
    let json = serde_json::to_string(keys).unwrap_or_else(|_| "[]".into());
    settings.set(SETTINGS_KEY_API_KEYS, &json).ok();
}

pub fn load_webhooks(settings: &SettingsRepo) -> Vec<Webhook> {
    settings
        .get(SETTINGS_KEY_WEBHOOKS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_webhooks(settings: &SettingsRepo, hooks: &[Webhook]) {
    let json = serde_json::to_string(hooks).unwrap_or_else(|_| "[]".into());
    settings.set(SETTINGS_KEY_WEBHOOKS, &json).ok();
}

/// Generate a `tunedev_` prefixed key with 32 random hex chars.
fn generate_dev_key() -> String {
    let hex = uuid::Uuid::new_v4().to_string().replace('-', "");
    format!("tunedev_{hex}")
}

fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO-8601 UTC using the `time` crate
    let dt = time::OffsetDateTime::from_unix_timestamp(now as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    dt.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| format!("{now}"))
}

// ---------------------------------------------------------------------------
// API key endpoints
// ---------------------------------------------------------------------------

/// `GET /developer/api-keys` — list active developer API keys.
async fn list_api_keys(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let keys = load_api_keys(&settings);

    // Redact full keys in listing — show prefix only
    let redacted: Vec<Value> = keys
        .iter()
        .map(|k| {
            let preview = if k.key.len() > 12 {
                format!("{}...", &k.key[..12])
            } else {
                k.key.clone()
            };
            json!({
                "id": k.id,
                "name": k.name,
                "key_preview": preview,
                "scopes": k.scopes,
                "created_at": k.created_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "api_keys": redacted,
        "count": redacted.len(),
    })))
}

/// `POST /developer/api-keys` — create a new developer API key.
async fn create_api_key(
    State(state): State<AppState>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    // Validate scopes
    for scope in &body.scopes {
        if !VALID_SCOPES.contains(&scope.as_str()) {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid scope: {scope}"),
                    "valid_scopes": VALID_SCOPES,
                })),
            )
                .into_response());
        }
    }

    if body.name.trim().is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response());
    }

    if body.scopes.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one scope is required"})),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut keys = load_api_keys(&settings);

    let new_key = DevApiKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        key: generate_dev_key(),
        scopes: body.scopes,
        created_at: now_iso(),
    };

    info!(name = %new_key.name, id = %new_key.id, "developer_api_key_created");

    let response = json!({
        "id": new_key.id,
        "name": new_key.name,
        "key": new_key.key,
        "scopes": new_key.scopes,
        "created_at": new_key.created_at,
    });

    keys.push(new_key);
    save_api_keys(&settings, &keys);

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/api-keys/{key_id}` — revoke a developer API key.
async fn revoke_api_key(
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut keys = load_api_keys(&settings);

    let before = keys.len();
    keys.retain(|k| k.id != key_id);

    if keys.len() == before {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "api key not found"})),
        )
            .into_response());
    }

    save_api_keys(&settings, &keys);
    info!(key_id = %key_id, "developer_api_key_revoked");

    Ok(Json(json!({"ok": true, "revoked": key_id})).into_response())
}

// ---------------------------------------------------------------------------
// Webhook endpoints
// ---------------------------------------------------------------------------

/// `GET /developer/webhooks` — list registered webhooks.
async fn list_webhooks(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let hooks = load_webhooks(&settings);

    Ok(Json(json!({
        "webhooks": hooks,
        "count": hooks.len(),
    })))
}

/// `POST /developer/webhooks` — register a webhook.
async fn create_webhook(
    State(state): State<AppState>,
    Json(body): Json<CreateWebhookRequest>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    // Validate URL
    let url = body.url.trim().to_string();
    if url.is_empty() || (!url.starts_with("http://") && !url.starts_with("https://")) {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "url must be a valid http(s) URL"})),
        )
            .into_response());
    }

    // Validate events
    for ev in &body.events {
        if !VALID_EVENTS.contains(&ev.as_str()) {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("invalid event: {ev}"),
                    "valid_events": VALID_EVENTS,
                })),
            )
                .into_response());
        }
    }

    if body.events.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one event is required"})),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut hooks = load_webhooks(&settings);

    let webhook = Webhook {
        id: uuid::Uuid::new_v4().to_string(),
        url,
        events: body.events,
        created_at: now_iso(),
    };

    info!(id = %webhook.id, url = %webhook.url, "developer_webhook_registered");

    let response = json!({
        "id": webhook.id,
        "url": webhook.url,
        "events": webhook.events,
        "created_at": webhook.created_at,
    });

    hooks.push(webhook);
    save_webhooks(&settings, &hooks);

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/webhooks/{id}` — remove a webhook.
async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut hooks = load_webhooks(&settings);

    let before = hooks.len();
    hooks.retain(|h| h.id != id);

    if hooks.len() == before {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "webhook not found"})),
        )
            .into_response());
    }

    save_webhooks(&settings, &hooks);
    info!(webhook_id = %id, "developer_webhook_removed");

    Ok(Json(json!({"ok": true, "removed": id})).into_response())
}

/// `POST /developer/webhooks/test` — send a test event to all webhooks.
async fn test_webhooks(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let hooks = load_webhooks(&settings);

    if hooks.is_empty() {
        return Ok(Json(json!({
            "sent": 0,
            "message": "no webhooks registered",
        }))
        .into_response());
    }

    let test_payload = json!({
        "event": "test",
        "timestamp": now_iso(),
        "data": {
            "message": "This is a test webhook event from Tune Developer API",
        },
    });

    let client = state.http_client.clone();
    let mut sent = 0u32;
    let mut errors = Vec::new();

    for hook in &hooks {
        let result = client
            .post(&hook.url)
            .header("Content-Type", "application/json")
            .header("User-Agent", "Tune-Webhook/2.0")
            .json(&test_payload)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 200 && status < 300 {
                    sent += 1;
                } else {
                    errors.push(json!({
                        "webhook_id": hook.id,
                        "url": hook.url,
                        "status": status,
                    }));
                }
            }
            Err(e) => {
                errors.push(json!({
                    "webhook_id": hook.id,
                    "url": hook.url,
                    "error": e.to_string(),
                }));
            }
        }
    }

    Ok(Json(json!({
        "sent": sent,
        "errors": errors,
        "total": hooks.len(),
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Webhook dispatcher — background task
// ---------------------------------------------------------------------------

/// Spawn a background task that listens for playback events and dispatches
/// matching events to registered webhook URLs.  Fire-and-forget: webhook
/// failures never block playback.
pub fn spawn_webhook_dispatcher(state: &AppState) {
    let playback = state.playback.clone();
    let backend = state.backend.clone();
    let http_client = state.http_client.clone();
    let event_bus = state.event_bus.clone();

    // Clone before moving into first spawn
    let backend2 = backend.clone();
    let http_client2 = http_client.clone();

    // Subscribe to playback events (broadcast channel)
    let mut playback_rx = playback.subscribe();

    tokio::spawn(async move {
        info!("webhook_dispatcher_started");
        loop {
            match playback_rx.recv().await {
                Ok(event) => {
                    // Map playback event names to webhook event names
                    let webhook_event = match event.event.as_str() {
                        "started" => "track.started",
                        "ended" | "finished" => "track.ended",
                        "volume_changed" => "volume.changed",
                        _ => continue,
                    };

                    let settings = SettingsRepo::with_backend(backend.clone());
                    let hooks = load_webhooks(&settings);

                    if hooks.is_empty() {
                        continue;
                    }

                    let matching: Vec<&Webhook> = hooks
                        .iter()
                        .filter(|h| h.events.contains(&webhook_event.to_string()))
                        .collect();

                    if matching.is_empty() {
                        continue;
                    }

                    let payload = json!({
                        "event": webhook_event,
                        "zone_id": event.zone_id,
                        "timestamp": now_iso(),
                        "data": event.data,
                    });

                    for hook in matching {
                        let client = http_client.clone();
                        let url = hook.url.clone();
                        let body = payload.clone();
                        // Fire-and-forget — don't block on webhook delivery
                        tokio::spawn(async move {
                            let result = client
                                .post(&url)
                                .header("Content-Type", "application/json")
                                .header("User-Agent", "Tune-Webhook/2.0")
                                .json(&body)
                                .timeout(std::time::Duration::from_secs(10))
                                .send()
                                .await;

                            if let Err(e) = result {
                                warn!(url = %url, error = %e, "webhook_delivery_failed");
                            }
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "webhook_dispatcher_lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("webhook_dispatcher_channel_closed");
                    break;
                }
            }
        }
    });

    // Also subscribe to the general EventBus for zone events
    let mut bus_rx = event_bus.subscribe();

    tokio::spawn(async move {
        loop {
            match bus_rx.recv().await {
                Ok(event) => {
                    let webhook_event = match event.event_type.as_str() {
                        "zone.created" | "zone.deleted" | "zone.updated" => "zone.changed",
                        _ => continue,
                    };

                    let settings = SettingsRepo::with_backend(backend2.clone());
                    let hooks = load_webhooks(&settings);

                    let matching: Vec<&Webhook> = hooks
                        .iter()
                        .filter(|h| h.events.contains(&webhook_event.to_string()))
                        .collect();

                    if matching.is_empty() {
                        continue;
                    }

                    let payload = json!({
                        "event": webhook_event,
                        "timestamp": now_iso(),
                        "data": event.data,
                    });

                    for hook in matching {
                        let client = http_client2.clone();
                        let url = hook.url.clone();
                        let body = payload.clone();
                        tokio::spawn(async move {
                            let result = client
                                .post(&url)
                                .header("Content-Type", "application/json")
                                .header("User-Agent", "Tune-Webhook/2.0")
                                .json(&body)
                                .timeout(std::time::Duration::from_secs(10))
                                .send()
                                .await;

                            if let Err(e) = result {
                                warn!(url = %url, error = %e, "webhook_delivery_failed");
                            }
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "webhook_bus_dispatcher_lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("webhook_bus_dispatcher_closed");
                    break;
                }
            }
        }
    });
}
