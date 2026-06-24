use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::cloud::plugins::PluginMarketplace;
use tune_core::cloud::sso::MozaikAuth;
use tune_core::cloud::telemetry::TelemetryReporter;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sso/authorize", get(sso_authorize))
        .route("/sso/callback", get(sso_callback))
        .route("/sso/status", get(sso_status))
        .route("/telemetry/status", get(telemetry_status))
        .route("/telemetry/enable", post(telemetry_enable))
        .route("/telemetry/disable", post(telemetry_disable))
        .route("/plugins", get(marketplace_list))
        .route("/plugins/{name}/install", post(marketplace_install))
        .route("/plugins/{name}/vote", post(marketplace_vote))
        .route("/community/artist-image", post(report_artist_image))
        .route("/community/genre-correction", post(submit_genre_correction))
        .route("/community/covers", post(submit_community_cover))
        .route("/community/covers/sync", post(sync_community_covers))
        .route("/bridge/status", get(bridge_status))
        .route("/bridge/enable", post(bridge_enable))
        .route("/bridge/disable", post(bridge_disable))
        .route("/license/status", get(license_status))
        .route("/license/activate", post(license_activate))
        .route("/license/deactivate", post(license_deactivate))
        .route("/license/validate", post(license_validate))
        .route("/library-sync/status", get(library_sync_status))
        .route("/library-sync/trigger", post(library_sync_trigger))
        .route("/library-sync/full-sync", post(library_sync_full))
}

// ---------------------------------------------------------------------------
// SSO
// ---------------------------------------------------------------------------

fn get_mozaik_auth(settings: &SettingsRepo) -> Option<MozaikAuth> {
    let client_id = settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_MOZAIK_CLIENT_ID").ok())
        .filter(|s| !s.is_empty())?;
    let base_url = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());
    Some(MozaikAuth::new(client_id, Some(&base_url)))
}

fn redirect_uri(state: &AppState) -> String {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get("mozaik_redirect_uri")
        .ok()
        .flatten()
        .unwrap_or_else(|| format!("http://localhost:{}/api/v1/cloud/sso/callback", state.port))
}

async fn sso_authorize(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let Some(auth) = get_mozaik_auth(&settings) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "SSO not configured — set mozaik_client_id in settings"})),
        )
            .into_response();
    };

    let uri = redirect_uri(&state);
    let url = auth.authorize_url(&uri);
    Redirect::temporary(&url).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

async fn sso_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
) -> impl IntoResponse {
    if let Some(err) = q.error {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("OAuth error: {err}")})),
        )
            .into_response();
    }

    let Some(code) = q.code else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing authorization code"})),
        )
            .into_response();
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let Some(auth) = get_mozaik_auth(&settings) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "SSO not configured"})),
        )
            .into_response();
    };

    let client_secret = settings
        .get("mozaik_client_secret")
        .ok()
        .flatten()
        .unwrap_or_default();

    let uri = redirect_uri(&state);

    let token = match auth.exchange_code(&code, &uri, &client_secret).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "sso_code_exchange_failed");
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
        }
    };

    // Fetch user profile from mozaiklabs
    let user = match auth.get_user(&token.access_token).await {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, "sso_user_fetch_failed");
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
        }
    };

    // Store cloud tokens in settings
    settings
        .set("mozaik_access_token", &token.access_token)
        .ok();
    if let Some(ref rt) = token.refresh_token {
        settings.set("mozaik_refresh_token", rt).ok();
    }
    settings
        .set(
            "mozaik_user",
            &serde_json::to_string(&user).unwrap_or_default(),
        )
        .ok();

    // Create or link local profile, then issue a local JWT session
    use tune_core::db::backend::ToSqlValue;
    let existing_id: Option<i64> = state
        .backend
        .query_one(
            "SELECT id FROM profiles WHERE email = ?",
            &[&user.email as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()));

    let profile_id = if let Some(id) = existing_id {
        // Update display name / avatar from cloud profile
        state
            .backend
            .execute(
                "UPDATE profiles SET display_name = ?, avatar_path = ? WHERE id = ?",
                &[
                    &user.display_name as &dyn ToSqlValue,
                    &user.avatar_url as &dyn ToSqlValue,
                    &id as &dyn ToSqlValue,
                ],
            )
            .ok();
        id
    } else {
        // Create new local profile from cloud user
        state
            .backend
            .execute(
                "INSERT INTO profiles (username, display_name, email, avatar_path, is_admin) VALUES (?, ?, ?, ?, ?)",
                &[&user.email as &dyn ToSqlValue, &user.display_name as &dyn ToSqlValue, &user.email as &dyn ToSqlValue, &user.avatar_url as &dyn ToSqlValue, &user.is_admin as &dyn ToSqlValue],
            )
            .ok();
        state.backend.last_insert_rowid()
    };

    let role = if user.is_admin { "admin" } else { "user" };
    let jwt_secret = match settings.get("jwt_secret").ok().flatten() {
        Some(s) if !s.is_empty() => s,
        _ => {
            let s = uuid::Uuid::new_v4().to_string();
            settings.set("jwt_secret", &s).ok();
            s
        }
    };

    let jwt = match crate::auth::sign_jwt(profile_id, role, &jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("JWT creation failed: {e}")})),
            )
                .into_response();
        }
    };

    info!(user_id = profile_id, email = %user.email, "sso_login_success");

    let cookie = format!("tune_session={jwt}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400");
    let mut response = Redirect::temporary("/").into_response();
    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    response
}

async fn sso_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let configured = settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .is_some();
    let connected = settings
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .is_some();
    let user: Option<tune_core::cloud::sso::CloudUser> = settings
        .get("mozaik_user")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    Json(json!({
        "configured": configured,
        "connected": connected,
        "user": user,
    }))
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

async fn telemetry_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = TelemetryReporter::is_enabled();
    let server_id = settings.get("server_id").ok().flatten();
    Json(json!({
        "enabled": enabled,
        "server_id": server_id,
    }))
}

async fn telemetry_enable(State(state): State<AppState>) -> Json<Value> {
    // Telemetry is now env-var-driven (TUNE_TELEMETRY=false to disable).
    // This endpoint creates/returns the server_id for informational purposes.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    TelemetryReporter::get_or_create_server_id(&settings);
    info!("telemetry_enabled");
    Json(json!({ "enabled": TelemetryReporter::is_enabled() }))
}

async fn telemetry_disable(State(_state): State<AppState>) -> Json<Value> {
    // Telemetry is now disabled via TUNE_TELEMETRY=false env var.
    info!("telemetry_disable_requested_use_env_var");
    Json(
        json!({ "enabled": TelemetryReporter::is_enabled(), "note": "Set TUNE_TELEMETRY=false to disable telemetry" }),
    )
}

// ---------------------------------------------------------------------------
// Plugin marketplace
// ---------------------------------------------------------------------------

async fn marketplace_list(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let mp = PluginMarketplace::new(base_url.as_deref());
    let plugins = mp.list().await;
    Json(json!(plugins))
}

async fn marketplace_install(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let mp = PluginMarketplace::new(base_url.as_deref());

    match mp.download(&name).await {
        Ok(data) => {
            // Store plugin data in the plugins directory
            let plugins_dir =
                std::env::var("TUNE_PLUGINS_DIR").unwrap_or_else(|_| "plugins".into());
            let plugin_dir = std::path::Path::new(&plugins_dir).join(&name);
            if let Err(e) = std::fs::create_dir_all(&plugin_dir) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("failed to create plugin dir: {e}")})),
                )
                    .into_response();
            }

            let archive_path = plugin_dir.join("plugin.tar.gz");
            if let Err(e) = std::fs::write(&archive_path, &data) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("failed to write plugin archive: {e}")})),
                )
                    .into_response();
            }

            // Track installation in settings
            settings
                .set(&format!("plugin_{name}_installed"), "true")
                .ok();
            settings.set(&format!("plugin_{name}_enabled"), "true").ok();

            info!(plugin = %name, size = data.len(), "marketplace_plugin_installed");
            Json(json!({
                "name": name,
                "status": "installed",
                "size": data.len(),
            }))
            .into_response()
        }
        Err(e) => {
            warn!(plugin = %name, error = %e, "marketplace_install_failed");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct VoteRequest {
    up: bool,
}

async fn marketplace_vote(
    Path(name): Path<String>,
    State(state): State<AppState>,
    Json(body): Json<VoteRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let mp = PluginMarketplace::new(base_url.as_deref());

    match mp.vote(&name, body.up).await {
        Ok(()) => Json(json!({"name": name, "voted": true, "up": body.up})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Community metadata
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ArtistImageReport {
    mbid: String,
    image_url: String,
}

async fn report_artist_image(
    State(state): State<AppState>,
    Json(body): Json<ArtistImageReport>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();

    match tune_core::cloud::community::report_artist_image(
        &body.mbid,
        &body.image_url,
        base_url.as_deref(),
    )
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct GenreCorrectionRequest {
    album_id: String,
    genre: String,
}

async fn submit_genre_correction(
    State(state): State<AppState>,
    Json(body): Json<GenreCorrectionRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();

    match tune_core::cloud::community::submit_genre_correction(
        &body.album_id,
        &body.genre,
        base_url.as_deref(),
    )
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Community covers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CoverSubmitRequest {
    mbid_release: String,
    album_title: String,
    artist_name: Option<String>,
    image_base64: String,
}

async fn submit_community_cover(
    State(state): State<AppState>,
    Json(body): Json<CoverSubmitRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Decode base64 image data
    let image_data = match base64_decode(&body.image_base64) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid base64: {e}")})),
            )
                .into_response();
        }
    };

    match tune_core::cloud::community::submit_cover(
        &base_url,
        &body.mbid_release,
        &body.album_title,
        body.artist_name.as_deref(),
        &instance_id,
        &image_data,
    )
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => {
            warn!(error = %e, "community_cover_submit_failed");
            (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct CoverSyncRequest {
    since: Option<String>,
}

async fn sync_community_covers(
    State(state): State<AppState>,
    Json(body): Json<CoverSyncRequest>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());

    let covers =
        match tune_core::cloud::community::fetch_approved_covers(&base_url, body.since.as_deref())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "community_covers_sync_failed");
                return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
            }
        };

    let client = tune_core::http::client::shared();
    let artwork_dir = &state.config.artwork_dir;
    if let Err(e) = std::fs::create_dir_all(artwork_dir) {
        warn!(error = %e, "artwork_cache_dir_create_failed");
    }

    let mut synced = 0u32;
    for cover in &covers {
        // Build the full image URL (relative paths need the base)
        let image_url = if cover.image_url.starts_with("http") {
            cover.image_url.clone()
        } else {
            format!("{}{}", base_url.trim_end_matches('/'), cover.image_url)
        };

        // Download the image
        let image_data = match client
            .get(&image_url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(b) => b,
                Err(_) => continue,
            },
            _ => continue,
        };

        // Save to artwork_cache/{mbid}.jpg
        let dest = std::path::Path::new(artwork_dir).join(format!("{}.jpg", cover.mbid_release));
        if std::fs::write(&dest, &image_data).is_err() {
            continue;
        }

        // Update album cover_path in DB for matching mbid
        let dest_str = dest.to_string_lossy().to_string();
        let mbid = cover.mbid_release.clone();
        state
            .backend
            .execute(
                "UPDATE albums SET cover_path = ? WHERE mbid = ? AND (cover_path IS NULL OR cover_path = '')",
                &[&dest_str as &dyn ToSqlValue, &mbid as &dyn ToSqlValue],
            )
            .ok();
        synced += 1;
    }

    info!(total = covers.len(), synced, "community_covers_synced");
    Json(json!({
        "total": covers.len(),
        "synced": synced,
    }))
    .into_response()
}

/// Minimal base64 decoder (standard alphabet, with padding).
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in TABLE.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }

    let cleaned: Vec<u8> = input
        .bytes()
        .filter(|b| *b != b'\n' && *b != b'\r' && *b != b' ')
        .collect();
    let stripped: &[u8] = if cleaned.ends_with(b"==") {
        &cleaned[..cleaned.len() - 2]
    } else if cleaned.ends_with(b"=") {
        &cleaned[..cleaned.len() - 1]
    } else {
        &cleaned
    };

    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    let chunks = stripped.chunks(4);
    for chunk in chunks {
        let mut buf = 0u32;
        for (i, &byte) in chunk.iter().enumerate() {
            let val = lookup[byte as usize];
            if val == 255 {
                return Err(format!("invalid base64 character: {}", byte as char));
            }
            buf |= (val as u32) << (18 - 6 * i);
        }
        let bytes_to_write = chunk.len() - 1;
        for i in 0..bytes_to_write {
            out.push((buf >> (16 - 8 * i)) as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Bridge (cloud relay)
// ---------------------------------------------------------------------------

async fn bridge_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = TelemetryReporter::get_or_create_server_id(&settings);
    let bridge_token = settings.get("bridge_token").ok().flatten();
    let bridge_url = settings
        .get("bridge_url")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_BRIDGE_URL").ok())
        .unwrap_or_else(|| "wss://bridge.mozaiklabs.fr/ws/server".to_string());

    let enabled = settings
        .get("bridge_enabled")
        .ok()
        .flatten()
        .map(|v| matches!(v.as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
        || std::env::var("TUNE_BRIDGE_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false);

    let connected = {
        #[cfg(feature = "cloud-relay")]
        {
            state
                .relay_client
                .as_ref()
                .map(|c| c.is_connected())
                .unwrap_or(false)
        }
        #[cfg(not(feature = "cloud-relay"))]
        {
            false
        }
    };

    Json(json!({
        "enabled": enabled,
        "connected": connected,
        "server_id": server_id,
        "relay_url": bridge_url,
        "has_token": bridge_token.is_some(),
        "access_url": if enabled {
            Some(format!("https://bridge.mozaiklabs.fr/{server_id}/"))
        } else {
            None
        },
    }))
}

async fn bridge_enable(State(state): State<AppState>) -> impl IntoResponse {
    // Premium gate: Cloud Relay requires Premium
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::CloudRelay,
    )
    .await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set("bridge_enabled", "true");

    let token = match settings.get("bridge_token").ok().flatten() {
        Some(t) if !t.is_empty() => t,
        _ => {
            let t = uuid::Uuid::new_v4().to_string();
            let _ = settings.set("bridge_token", &t);
            t
        }
    };

    let server_id = TelemetryReporter::get_or_create_server_id(&settings);
    info!(server_id = %server_id, "bridge enabled");

    Json(json!({
        "enabled": true,
        "server_id": server_id,
        "bridge_token": token,
        "access_url": format!("https://bridge.mozaiklabs.fr/{server_id}/"),
        "note": "restart server to activate the relay connection"
    }))
    .into_response()
}

async fn bridge_disable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let _ = settings.set("bridge_enabled", "false");
    info!("bridge disabled");
    Json(json!({"enabled": false}))
}

// ---------------------------------------------------------------------------
// License
// ---------------------------------------------------------------------------

async fn license_status(State(state): State<AppState>) -> Json<Value> {
    let ls = state.license.license_state().await;
    let mut features = serde_json::Map::new();
    for f in tune_core::license::Feature::all_premium() {
        let enabled = state.license.check_feature(*f).await;
        features.insert(
            serde_json::to_value(f)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string(),
            json!({
                "display_name": f.display_name(),
                "enabled": enabled,
            }),
        );
    }

    let zone_limit = if ls.tier == tune_core::license::Tier::Premium {
        None
    } else {
        Some(tune_core::license::LicenseManager::free_zone_limit())
    };

    Json(json!({
        "tier": ls.tier,
        "license_key": ls.license_key,
        "expires_at": ls.expires_at,
        "last_validated": ls.last_validated,
        "hardware_fingerprint": ls.hardware_fingerprint,
        "features": features,
        "zone_limit": zone_limit,
    }))
}

async fn license_activate(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let key = body["license_key"].as_str().unwrap_or("");
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "license_key required"})),
        )
            .into_response();
    }
    if let Err(e) = state.license.set_license_key(key).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }
    let ls = state.license.license_state().await;
    state.event_bus.emit(
        "license.updated",
        json!({"tier": ls.tier, "expires_at": ls.expires_at}),
    );
    Json(json!({
        "status": "activated",
        "tier": ls.tier,
        "license_key": key,
    }))
    .into_response()
}

async fn license_deactivate(State(state): State<AppState>) -> Json<Value> {
    state.license.clear_license().await;
    state.event_bus.emit(
        "license.updated",
        json!({"tier": "free", "expires_at": null}),
    );
    Json(json!({"status": "deactivated", "tier": "free"}))
}

/// POST /cloud/license/validate
///
/// Triggers an immediate license validation against mozaiklabs.fr.
/// Returns the authoritative tier from the server, or the cached state
/// if the server is unreachable (graceful degradation).
async fn license_validate(State(state): State<AppState>) -> impl IntoResponse {
    let ls = state.license.license_state().await;
    let Some(ref key) = ls.license_key else {
        return Json(json!({
            "status": "no_license",
            "tier": "free",
            "message": "No license key configured",
        }))
        .into_response();
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();

    let payload = json!({
        "license_key": key,
        "hardware_fingerprint": ls.hardware_fingerprint,
        "server_id": server_id,
        "version": tune_core::version(),
    });

    let resp = match state
        .http_client
        .post("https://mozaiklabs.fr/api/v1/license/validate")
        .timeout(std::time::Duration::from_secs(10))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "license_validate_request_failed");
            return Json(json!({
                "status": "error",
                "tier": ls.tier,
                "message": format!("Validation request failed: {e}"),
                "cached": true,
            }))
            .into_response();
        }
    };

    // 404 means the server endpoint doesn't exist yet — keep cached state.
    if resp.status() == StatusCode::NOT_FOUND {
        info!("license_validate_endpoint_not_found, keeping cached state");
        return Json(json!({
            "status": "cached",
            "tier": ls.tier,
            "message": "Validation endpoint not available yet",
            "cached": true,
        }))
        .into_response();
    }

    if !resp.status().is_success() {
        let status = resp.status();
        warn!(status = %status, "license_validate_server_error");
        return Json(json!({
            "status": "error",
            "tier": ls.tier,
            "message": format!("Server returned {status}"),
            "cached": true,
        }))
        .into_response();
    }

    // Parse the server's authoritative response.
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "license_validate_parse_failed");
            return Json(json!({
                "status": "error",
                "tier": ls.tier,
                "message": format!("Failed to parse response: {e}"),
                "cached": true,
            }))
            .into_response();
        }
    };

    let valid = body
        .get("license_valid")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if !valid {
        info!("license_invalidated_by_server_validate");
        state
            .license
            .update_from_server(tune_core::license::Tier::Free, None)
            .await;
        state.event_bus.emit(
            "license.updated",
            json!({"tier": "free", "expires_at": null}),
        );
        return Json(json!({
            "status": "invalid",
            "tier": "free",
            "message": "License key is not valid",
        }))
        .into_response();
    }

    let tier_str = body
        .get("license_tier")
        .and_then(|v| v.as_str())
        .unwrap_or("free");
    let tier = match tier_str {
        "premium" => tune_core::license::Tier::Premium,
        _ => tune_core::license::Tier::Free,
    };
    let expires_at = body
        .get("license_expires_at")
        .and_then(|v| v.as_str())
        .map(String::from);

    state
        .license
        .update_from_server(tier, expires_at.clone())
        .await;
    info!(tier = %tier, "license_validated_on_demand");
    state.event_bus.emit(
        "license.updated",
        json!({"tier": tier, "expires_at": expires_at}),
    );

    let updated = state.license.license_state().await;
    Json(json!({
        "status": "validated",
        "tier": updated.tier,
        "expires_at": updated.expires_at,
        "last_validated": updated.last_validated,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Cloud Library Sync
// ---------------------------------------------------------------------------

/// GET /cloud/library-sync/status — returns pending count, last sync time, enabled state.
async fn library_sync_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let pending = tune_core::cloud::library_sync::pending_count(&state.backend);
    let last_sync = settings.get("cloud_library_last_sync").ok().flatten();
    let has_token = settings
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .is_some();
    let is_premium = state.license.is_premium().await;

    Json(json!({
        "enabled": is_premium && has_token,
        "pending": pending,
        "last_sync": last_sync,
        "is_premium": is_premium,
        "has_token": has_token,
    }))
}

/// POST /cloud/library-sync/trigger — triggers immediate sync (premium only).
async fn library_sync_trigger(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::CloudBackup,
    )
    .await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
    let token = match settings.get("mozaik_access_token").ok().flatten() {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({"error": "No Mozaik access token — log in via SSO first"})),
            )
                .into_response();
        }
    };

    if server_id.is_empty() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "No server_id configured"})),
        )
            .into_response();
    }

    let pending = tune_core::cloud::library_sync::pending_count(&state.backend);
    if pending == 0 {
        return Json(json!({
            "status": "nothing_to_sync",
            "pending": 0,
        }))
        .into_response();
    }

    // Spawn the sync in the background so the request returns immediately
    let backend = state.backend.clone();
    let http_client = state.http_client.clone();
    tokio::spawn(async move {
        match tune_core::cloud::library_sync::push_changes(
            &backend,
            &http_client,
            &server_id,
            &token,
        )
        .await
        {
            Ok(report) => {
                let settings = SettingsRepo::with_backend(backend.clone());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string();
                settings.set("cloud_library_last_sync", &now).ok();
                info!(
                    tracks = report.tracks_synced,
                    albums = report.albums_synced,
                    artists = report.artists_synced,
                    "cloud_library_sync_triggered_complete"
                );
            }
            Err(e) => {
                warn!(error = %e, "cloud_library_sync_triggered_failed");
            }
        }
    });

    Json(json!({
        "status": "sync_triggered",
        "pending": pending,
    }))
    .into_response()
}

/// POST /cloud/library-sync/full-sync — queues a full library resync (premium only).
async fn library_sync_full(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::CloudBackup,
    )
    .await
    {
        return resp;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
    let token = match settings.get("mozaik_access_token").ok().flatten() {
        Some(t) if !t.is_empty() => t,
        _ => {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({"error": "No Mozaik access token — log in via SSO first"})),
            )
                .into_response();
        }
    };

    if server_id.is_empty() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"error": "No server_id configured"})),
        )
            .into_response();
    }

    // Spawn the full sync in the background
    let backend = state.backend.clone();
    let http_client = state.http_client.clone();
    tokio::spawn(async move {
        match tune_core::cloud::library_sync::full_sync(&backend, &http_client, &server_id, &token)
            .await
        {
            Ok(report) => {
                let settings = SettingsRepo::with_backend(backend.clone());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string();
                settings.set("cloud_library_last_sync", &now).ok();
                info!(
                    tracks = report.tracks_synced,
                    albums = report.albums_synced,
                    artists = report.artists_synced,
                    errors = report.errors.len(),
                    "cloud_library_full_sync_triggered_complete"
                );
            }
            Err(e) => {
                warn!(error = %e, "cloud_library_full_sync_triggered_failed");
            }
        }
    });

    Json(json!({
        "status": "full_sync_queued",
        "message": "Full library sync has been queued in the background",
    }))
    .into_response()
}
