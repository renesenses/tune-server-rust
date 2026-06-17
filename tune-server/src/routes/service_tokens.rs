use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use tracing::error;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::services_manager::ServicesManager;

use crate::state::AppState;

pub async fn list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let registry = state.services.lock().await;
    let streaming_status = registry.status_all().await;
    drop(registry);

    let tidal_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "tidal")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let qobuz_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "qobuz")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let spotify_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "spotify")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);
    let deezer_auth = streaming_status
        .iter()
        .find(|s| s["name"] == "deezer")
        .and_then(|s| s["authenticated"].as_bool())
        .unwrap_or(false);

    // Last.fm scrobbling state
    let lastfm_configured = settings.get("lastfm_api_key").ok().flatten().is_some();
    let lastfm_session_key = settings
        .get("lastfm_session_key")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lastfm_authenticated = lastfm_session_key.is_some();
    let lastfm_username = settings
        .get("lastfm_username")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    let lastfm_scrobble_enabled = settings
        .get("lastfm_scrobble_enabled")
        .ok()
        .flatten()
        .as_deref()
        != Some("false"); // default to true when authenticated

    // Validation state from services_manager (stored in streaming_auth via validate_and_save)
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());
    let discogs_payload = svc_mgr.load_token("discogs").ok().flatten();
    let lastfm_payload = svc_mgr.load_token("lastfm").ok().flatten();
    let genius_payload = svc_mgr.load_token("genius").ok().flatten();

    let discogs_db_configured = settings.get("discogs_token").ok().flatten().is_some()
        || state
            .config
            .discogs_token
            .as_deref()
            .is_some_and(|s| !s.is_empty());
    let discogs_configured = discogs_db_configured || discogs_payload.is_some();

    let services = vec![
        json!({
            "id": "musicbrainz", "name": "MusicBrainz", "kind": "no_auth",
            "purpose": "Années + crédits + couvertures (ID releases).",
            "pricing": "free", "pricing_note": "100 % gratuit, base de données ouverte.",
            "configured": true, "source": serde_json::Value::Null, "valid": true,
            "validated_at": serde_json::Value::Null, "validation_message": serde_json::Value::Null,
            "fields": [],
            "help_url": "https://musicbrainz.org/",
            "help_steps": ["Aucun token requis — MusicBrainz est gratuit et anonyme."],
        }),
        json!({
            "id": "discogs", "name": "Discogs", "kind": "personal_token",
            "purpose": "Années + couvertures + crédits pour pressages obscurs.",
            "pricing": "free", "pricing_note": "Compte + token personnel gratuits ; API gratuite avec quota (60 req/min).",
            "configured": discogs_configured,
            "source": if discogs_payload.is_some() { json!("db") } else if discogs_db_configured { json!("env") } else { serde_json::Value::Null },
            "valid": discogs_payload.as_ref().and_then(|p| p.valid),
            "validated_at": discogs_payload.as_ref().and_then(|p| p.validated_at),
            "validation_message": discogs_payload.as_ref().and_then(|p| p.validation_message.clone()),
            "fields": [{"key": "token", "label": "Personal Access Token", "type": "password"}],
            "help_url": "https://www.discogs.com/settings/developers",
            "help_steps": ["Connecte-toi sur discogs.com.", "Va dans Settings → Developers.", "Clique 'Generate new token'.", "Colle le token ici."],
        }),
        json!({
            "id": "lastfm", "name": "Last.fm", "kind": "api_key",
            "purpose": "Genres + scrobbling.",
            "pricing": "free", "pricing_note": "API gratuite pour usage non commercial.",
            "configured": lastfm_configured || lastfm_payload.is_some(),
            "source": if lastfm_payload.is_some() { json!("db") } else if lastfm_configured { json!("env") } else { serde_json::Value::Null },
            "valid": lastfm_payload.as_ref().and_then(|p| p.valid),
            "validated_at": lastfm_payload.as_ref().and_then(|p| p.validated_at),
            "validation_message": lastfm_payload.as_ref().and_then(|p| p.validation_message.clone()),
            "scrobble_authenticated": lastfm_authenticated,
            "scrobble_enabled": lastfm_authenticated && lastfm_scrobble_enabled,
            "lastfm_username": lastfm_username,
            "fields": [
                {"key": "api_key", "label": "API Key", "type": "text"},
                {"key": "api_secret", "label": "API Secret (pour scrobbling)", "type": "password"},
            ],
            "help_url": "https://www.last.fm/api/account/create",
            "help_steps": ["Va sur last.fm/api/account/create", "Renseigne un nom d'application.", "Récupère 'API key' et 'Shared secret'.", "Colle ici puis Enregistrer."],
        }),
        json!({
            "id": "genius", "name": "Genius", "kind": "api_key",
            "purpose": "Paroles.",
            "pricing": "free", "pricing_note": "API gratuite.",
            "configured": settings.get("genius_token").ok().flatten().is_some() || genius_payload.is_some(),
            "source": if genius_payload.is_some() { json!("db") } else if settings.get("genius_token").ok().flatten().is_some() { json!("env") } else { serde_json::Value::Null },
            "valid": genius_payload.as_ref().and_then(|p| p.valid),
            "validated_at": genius_payload.as_ref().and_then(|p| p.validated_at),
            "validation_message": genius_payload.as_ref().and_then(|p| p.validation_message.clone()),
            "fields": [{"key": "token", "label": "Access Token", "type": "password"}],
            "help_url": "https://genius.com/api-clients",
            "help_steps": ["Crée un compte sur genius.com.", "Va dans API Clients.", "Crée une application et copie le token."],
        }),
        json!({
            "id": "tidal", "name": "Tidal", "kind": "oauth",
            "purpose": "Streaming hi-res + années + couvertures.",
            "pricing": "paid", "pricing_note": "Abonnement Tidal HiFi requis (≈ 11€/mois).",
            "configured": tidal_auth, "source": if tidal_auth { json!("db") } else { serde_json::Value::Null },
            "valid": if tidal_auth { json!(true) } else { serde_json::Value::Null },
            "validated_at": serde_json::Value::Null, "validation_message": serde_json::Value::Null,
            "fields": [],
            "help_url": "/streaming/tidal",
            "help_steps": ["Tidal utilise OAuth — utilise la page Streaming → Tidal pour te connecter."],
        }),
        json!({
            "id": "qobuz", "name": "Qobuz", "kind": "login_password",
            "purpose": "Streaming hi-res + années + couvertures.",
            "pricing": "paid", "pricing_note": "Abonnement Qobuz Studio requis (≈ 13€/mois).",
            "configured": qobuz_auth, "source": if qobuz_auth { json!("db") } else { serde_json::Value::Null },
            "valid": if qobuz_auth { json!(true) } else { serde_json::Value::Null },
            "validated_at": serde_json::Value::Null, "validation_message": serde_json::Value::Null,
            "fields": [],
            "help_url": "/streaming/qobuz",
            "help_steps": ["Qobuz utilise login/password — utilise la page Streaming → Qobuz pour te connecter."],
        }),
        json!({
            "id": "spotify", "name": "Spotify", "kind": "oauth",
            "purpose": "Streaming + connectivité.",
            "pricing": "freemium", "pricing_note": "Compte Spotify gratuit ou Premium (≈ 11€/mois).",
            "configured": spotify_auth, "source": if spotify_auth { json!("db") } else { serde_json::Value::Null },
            "valid": if spotify_auth { json!(true) } else { serde_json::Value::Null },
            "validated_at": serde_json::Value::Null, "validation_message": serde_json::Value::Null,
            "fields": [],
            "help_url": "/streaming/spotify",
            "help_steps": ["Spotify utilise OAuth — utilise la page Streaming → Spotify pour te connecter."],
        }),
        json!({
            "id": "deezer", "name": "Deezer", "kind": "arl_token",
            "purpose": "Streaming.",
            "pricing": "freemium", "pricing_note": "Compte gratuit ou Deezer HiFi (≈ 12€/mois) pour FLAC.",
            "configured": deezer_auth,
            "source": if deezer_auth { json!("db") } else { serde_json::Value::Null },
            "valid": if deezer_auth { json!(true) } else { serde_json::Value::Null },
            "validated_at": serde_json::Value::Null, "validation_message": serde_json::Value::Null,
            "fields": [{"key": "arl", "label": "ARL token (depuis cookies deezer.com)", "type": "password"}],
            "help_url": "/streaming/deezer",
            "help_steps": ["Connecte-toi sur deezer.com.", "DevTools (F12) → Application → Cookies → cherche 'arl'.", "Colle le token ARL ici."],
        }),
    ];
    Json(json!(services))
}

pub async fn save(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Also write fields to settings table for backward compat (lastfm_auth_token etc.)
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            let skey = format!("{}_{}", id, key);
            let sval = value.as_str().unwrap_or("");
            if !sval.is_empty() {
                if let Err(e) = settings.set(&skey, sval) {
                    error!(key = %skey, error = %e, "service_token_save_failed");
                    return Json(json!({
                        "valid": false,
                        "validation_message": format!("Erreur sauvegarde: {e}")
                    }));
                }
            }
        }
    }

    // Validate and store in streaming_auth via services_manager (for fix-genres, list status)
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());
    let fields: std::collections::HashMap<String, serde_json::Value> = body
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(_, v)| !v.as_str().unwrap_or("").is_empty())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    if fields.is_empty() {
        return Json(json!({"valid": false, "validation_message": "Aucune valeur fournie"}));
    }

    let payload = tune_core::services_manager::TokenPayload {
        fields,
        valid: None,
        validation_message: None,
        validated_at: None,
    };

    match svc_mgr.validate_and_save(&id, payload).await {
        Ok((valid, msg)) => Json(json!({
            "valid": valid,
            "validation_message": msg,
        })),
        Err(e) => {
            error!(service = %id, error = %e, "service_token_validate_failed");
            Json(json!({
                "valid": false,
                "validation_message": format!("Erreur: {e}"),
            }))
        }
    }
}

pub async fn test(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());

    match id.as_str() {
        "musicbrainz" => Json(json!({
            "valid": true,
            "validation_message": "MusicBrainz disponible (aucun token requis).",
        })),
        "discogs" => {
            let token = svc_mgr
                .get_credential("discogs", "token")
                .or_else(|| settings.get("discogs_token").ok().flatten())
                .or_else(|| state.config.discogs_token.clone().filter(|s| !s.is_empty()));
            match token {
                Some(t) => {
                    let (valid, msg) = svc_mgr.validate_discogs(&t).await;
                    Json(json!({ "valid": valid, "validation_message": msg }))
                }
                None => Json(
                    json!({ "valid": false, "validation_message": "Token Discogs non configuré." }),
                ),
            }
        }
        "lastfm" => {
            let api_key = svc_mgr
                .get_credential("lastfm", "api_key")
                .or_else(|| settings.get("lastfm_api_key").ok().flatten());
            match api_key {
                Some(k) => {
                    let (valid, msg) = svc_mgr.validate_lastfm(&k).await;
                    Json(json!({ "valid": valid, "validation_message": msg }))
                }
                None => Json(
                    json!({ "valid": false, "validation_message": "API Key Last.fm non configurée." }),
                ),
            }
        }
        "genius" => {
            let configured = svc_mgr.get_credential("genius", "token").is_some()
                || settings.get("genius_token").ok().flatten().is_some();
            Json(json!({
                "valid": configured,
                "validation_message": if configured { "Token configuré (validation non disponible)." } else { "Token Genius non configuré." },
            }))
        }
        _ => Json(json!({
            "valid": serde_json::Value::Null,
            "validation_message": "Validation non disponible pour ce service.",
        })),
    }
}

pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let keys: Vec<String> = settings
        .all()
        .unwrap_or_default()
        .into_iter()
        .filter(|(k, _)| k.starts_with(&format!("{}_", id)))
        .map(|(k, _)| k)
        .collect();
    for k in &keys {
        settings.delete(k).ok();
    }
    // Also remove from streaming_auth table (saved by validate_and_save)
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());
    svc_mgr.delete_token(&id).ok();
    StatusCode::NO_CONTENT
}

/// Step 1: generate a Last.fm auth token and return the auth URL.
pub async fn lastfm_auth_token(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());

    // Check services_manager (streaming_auth) first, fall back to settings table
    let api_key = svc_mgr
        .get_credential("lastfm", "api_key")
        .or_else(|| settings.get("lastfm_api_key").ok().flatten())
        .filter(|s| !s.is_empty());
    let api_key = match api_key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "lastfm_api_key not configured — save your API key first in Services & Tokens"})),
            )
                .into_response();
        }
    };

    let api_secret = svc_mgr
        .get_credential("lastfm", "api_secret")
        .or_else(|| settings.get("lastfm_api_secret").ok().flatten())
        .filter(|s| !s.is_empty());
    let api_secret = match api_secret {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "lastfm_api_secret not configured — save your API secret first in Services & Tokens"})),
            )
                .into_response();
        }
    };

    match tune_core::scrobble::get_auth_token(&api_key, &api_secret).await {
        Ok(token) => {
            let auth_url = tune_core::scrobble::auth_url(&api_key, &token);
            Json(json!({ "token": token, "auth_url": auth_url })).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Step 2: exchange the token for a session key after user authorized on Last.fm.
pub async fn lastfm_auth_session(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let token = match body["token"].as_str() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing token"})),
            )
                .into_response();
        }
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let svc_mgr = ServicesManager::with_backend(state.backend.clone());

    // Check services_manager first, fall back to settings table
    let api_key = svc_mgr
        .get_credential("lastfm", "api_key")
        .or_else(|| settings.get("lastfm_api_key").ok().flatten())
        .filter(|s| !s.is_empty());
    let api_key = match api_key {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "lastfm_api_key not configured"})),
            )
                .into_response();
        }
    };
    let api_secret = svc_mgr
        .get_credential("lastfm", "api_secret")
        .or_else(|| settings.get("lastfm_api_secret").ok().flatten())
        .filter(|s| !s.is_empty());
    let api_secret = match api_secret {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "lastfm_api_secret not configured"})),
            )
                .into_response();
        }
    };
    match tune_core::scrobble::get_session_full(&api_key, &api_secret, &token).await {
        Ok(session) => {
            settings.set("lastfm_session_key", &session.key).ok();
            if !session.name.is_empty() {
                settings.set("lastfm_username", &session.name).ok();
            }
            // Enable scrobbling by default on successful auth
            settings.set("lastfm_scrobble_enabled", "true").ok();
            Json(json!({
                "ok": true,
                "session_key": session.key,
                "username": session.name,
                "scrobble_enabled": true,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Legacy endpoint kept for backward compatibility.
pub async fn lastfm_auth(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    lastfm_auth_session(State(state), Json(body)).await
}

/// Toggle scrobbling on/off.
pub async fn lastfm_scrobble_toggle(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = body["enabled"].as_bool().unwrap_or(false);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "lastfm_scrobble_enabled",
            if enabled { "true" } else { "false" },
        )
        .ok();
    Json(json!({ "ok": true, "scrobble_enabled": enabled }))
}

/// Disconnect Last.fm: remove session key, username, and disable scrobbling.
pub async fn lastfm_disconnect(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.delete("lastfm_session_key").ok();
    settings.delete("lastfm_username").ok();
    settings.set("lastfm_scrobble_enabled", "false").ok();
    Json(json!({ "ok": true }))
}
