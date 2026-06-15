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
    let enabled = TelemetryReporter::is_enabled(&settings);
    let instance_id = settings.get("instance_id").ok().flatten();
    Json(json!({
        "enabled": enabled,
        "instance_id": instance_id,
    }))
}

async fn telemetry_enable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("telemetry_enabled", "true").ok();
    TelemetryReporter::get_or_create_instance_id(&settings);
    info!("telemetry_enabled");
    Json(json!({ "enabled": true }))
}

async fn telemetry_disable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("telemetry_enabled", "false").ok();
    info!("telemetry_disabled");
    Json(json!({ "enabled": false }))
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
