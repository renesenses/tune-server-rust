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
use tune_http_types::panne_sql::OuDefautJournalise;

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

fn load_api_keys(settings: &SettingsRepo) -> Result<Vec<DevApiKey>, String> {
    settings.get_json_list(SETTINGS_KEY_API_KEYS)
}

pub fn load_webhooks(settings: &SettingsRepo) -> Result<Vec<Webhook>, String> {
    settings.get_json_list(SETTINGS_KEY_WEBHOOKS)
}

/// Une panne de stockage se dit, elle ne se déguise pas en liste vide (#2795).
///
/// Le corps ne porte jamais la valeur stockée : `detail` vient des messages de
/// `SettingsRepo`, de `serde_json` (position seule) et du pilote SQL (jamais
/// les paramètres liés). Aucune clef d'API n'y transite — le test
/// `cles_developpeur_persistance.rs` le vérifie sur une base volontairement
/// corrompue avec une clef à l'intérieur.
fn panne_de_stockage(quoi: &str, erreur: String) -> axum::response::Response {
    // La valeur n'est PAS journalisée, seulement la cause.
    warn!(quoi, erreur = %erreur, "developer_api_stockage_en_echec");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": "storage_failure",
            "detail": erreur,
        })),
    )
        .into_response()
}

/// L'origine d'une URL de webhook — schéma et hôte, rien de plus.
///
/// Une adresse de webhook est un secret en soi : chez Slack ou Discord, le
/// chemin (`/services/T…/B…/…`) EST le jeton, et quiconque l'a peut publier.
/// Le journal doit donc pouvoir nommer la destination sans la livrer. C'est la
/// même règle que pour les clefs d'API : jamais de secret dans une trace, même
/// tronqué.
pub fn origine_seule(url: &str) -> String {
    let (schema, reste) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        // Pas de schéma reconnaissable : on ne devine pas, on ne cite rien.
        None => return "(adresse illisible)".to_string(),
    };
    let hote = reste
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        // Un éventuel `user:motdepasse@` est lui aussi un secret.
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if hote.is_empty() {
        return "(adresse illisible)".to_string();
    }
    format!("{schema}://{hote}")
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
    let keys = match load_api_keys(&settings) {
        Ok(k) => k,
        Err(e) => return Ok(panne_de_stockage("lecture_cles", e)),
    };

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
    }))
    .into_response())
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

    let new_key = DevApiKey {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name.trim().to_string(),
        key: generate_dev_key(),
        scopes: body.scopes,
        created_at: now_iso(),
    };

    // La lecture, l'ajout et l'écriture dans UNE transaction, un écrivain à la
    // fois : deux créations simultanées se retrouvent toutes les deux dans la
    // liste. Avant la #2795, la seconde réécrivait la liste lue avant que la
    // première ne s'y ajoute — et le client repartait avec une clef affichée
    // une fois, jamais persistée.
    let ajoutee = new_key.clone();
    if let Err(e) = settings.update_json_list::<DevApiKey, _, _>(SETTINGS_KEY_API_KEYS, move |k| {
        k.push(ajoutee);
        Ok(())
    }) {
        return Ok(panne_de_stockage("creation_cle", e));
    }

    // Journalisée APRÈS la persistance, et sans la clef : une trace ne doit
    // annoncer que ce qui est vrai, et jamais porter un secret.
    info!(name = %new_key.name, id = %new_key.id, "developer_api_key_created");

    let response = json!({
        "id": new_key.id,
        "name": new_key.name,
        "key": new_key.key,
        "scopes": new_key.scopes,
        "created_at": new_key.created_at,
    });

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/api-keys/{key_id}` — revoke a developer API key.
async fn revoke_api_key(
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let cible = key_id.clone();
    let trouvee =
        match settings.update_json_list::<DevApiKey, _, _>(SETTINGS_KEY_API_KEYS, move |keys| {
            let avant = keys.len();
            keys.retain(|k| k.id != cible);
            Ok(keys.len() != avant)
        }) {
            Ok(t) => t,
            // Une révocation annoncée sans effet est pire que le refus : la
            // clef reste valable et son propriétaire la croit morte.
            Err(e) => return Ok(panne_de_stockage("revocation_cle", e)),
        };

    if !trouvee {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "api key not found"})),
        )
            .into_response());
    }

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
    let hooks = match load_webhooks(&settings) {
        Ok(h) => h,
        Err(e) => return Ok(panne_de_stockage("lecture_webhooks", e)),
    };

    Ok(Json(json!({
        "webhooks": hooks,
        "count": hooks.len(),
    }))
    .into_response())
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

    let webhook = Webhook {
        id: uuid::Uuid::new_v4().to_string(),
        url,
        events: body.events,
        created_at: now_iso(),
    };

    let ajoute = webhook.clone();
    if let Err(e) = settings.update_json_list::<Webhook, _, _>(SETTINGS_KEY_WEBHOOKS, move |h| {
        h.push(ajoute);
        Ok(())
    }) {
        return Ok(panne_de_stockage("creation_webhook", e));
    }

    info!(
        id = %webhook.id,
        origine = %origine_seule(&webhook.url),
        "developer_webhook_registered"
    );

    let response = json!({
        "id": webhook.id,
        "url": webhook.url,
        "events": webhook.events,
        "created_at": webhook.created_at,
    });

    Ok((StatusCode::CREATED, Json(response)).into_response())
}

/// `DELETE /developer/webhooks/{id}` — remove a webhook.
async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());

    let cible = id.clone();
    let trouve =
        match settings.update_json_list::<Webhook, _, _>(SETTINGS_KEY_WEBHOOKS, move |hooks| {
            let avant = hooks.len();
            hooks.retain(|h| h.id != cible);
            Ok(hooks.len() != avant)
        }) {
            Ok(t) => t,
            Err(e) => return Ok(panne_de_stockage("suppression_webhook", e)),
        };

    if !trouve {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "webhook not found"})),
        )
            .into_response());
    }

    info!(webhook_id = %id, "developer_webhook_removed");

    Ok(Json(json!({"ok": true, "removed": id})).into_response())
}

/// `POST /developer/webhooks/test` — send a test event to all webhooks.
async fn test_webhooks(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, axum::response::Response> {
    crate::premium_guard::require_premium(&state.license, Feature::DeveloperApi).await?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let hooks = match load_webhooks(&settings) {
        Ok(h) => h,
        // « 0 envoyé, aucun webhook enregistré » sur une base en panne
        // enverrait l'utilisateur reconfigurer ce qui existe déjà.
        Err(e) => return Ok(panne_de_stockage("lecture_webhooks", e)),
    };

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
                    // Ici, et ici seulement, la liste vide reste acceptable :
                    // le distributeur est « au mieux » et ne doit pas mourir
                    // sur une panne passagère. Mais elle ne sera plus
                    // silencieuse — et surtout, ce chemin n'ÉCRIT rien, donc
                    // il ne peut pas remplacer les webhooks par `[]`.
                    let hooks = load_webhooks(&settings).ou_defaut_journalise();

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
                                warn!(
                                    origine = %origine_seule(&url),
                                    error = %e,
                                    "webhook_delivery_failed"
                                );
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
                    let hooks = load_webhooks(&settings).ou_defaut_journalise();

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
                                warn!(
                                    origine = %origine_seule(&url),
                                    error = %e,
                                    "webhook_delivery_failed"
                                );
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
