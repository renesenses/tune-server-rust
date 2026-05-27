use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// HMAC-SHA256 helpers (no external crate needed)
// ---------------------------------------------------------------------------

fn hmac_sha256(data: &[u8], key: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    let result = outer.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

fn base64url_encode(data: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let lut = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut i = 0;
    while i + 2 < data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        let _ = write!(
            out,
            "{}{}{}{}",
            lut[((n >> 18) & 63) as usize] as char,
            lut[((n >> 12) & 63) as usize] as char,
            lut[((n >> 6) & 63) as usize] as char,
            lut[(n & 63) as usize] as char,
        );
        i += 3;
    }

    let rem = data.len() - i;
    if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        let _ = write!(
            out,
            "{}{}{}",
            lut[((n >> 18) & 63) as usize] as char,
            lut[((n >> 12) & 63) as usize] as char,
            lut[((n >> 6) & 63) as usize] as char,
        );
    } else if rem == 1 {
        let n = (data[i] as u32) << 16;
        let _ = write!(
            out,
            "{}{}",
            lut[((n >> 18) & 63) as usize] as char,
            lut[((n >> 12) & 63) as usize] as char,
        );
    }

    out
}

fn base64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let lut: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        .to_vec();

    let bytes: Vec<u8> = s.bytes().collect();
    let mut buf = 0u32;
    let mut bits = 0u8;

    for &b in &bytes {
        let val = lut.iter().position(|&c| c == b).ok_or("invalid base64url char")? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// JWT creation and verification
// ---------------------------------------------------------------------------

fn create_jwt(username: &str, secret: &str) -> String {
    let header = base64url_encode(r#"{"alg":"HS256","typ":"JWT"}"#.as_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let payload_str = format!(
        r#"{{"sub":"{}","iat":{},"exp":{}}}"#,
        username,
        now,
        now + 86400
    );
    let payload = base64url_encode(payload_str.as_bytes());
    let signing_input = format!("{header}.{payload}");
    let sig = hmac_sha256(signing_input.as_bytes(), secret.as_bytes());
    let sig_b64 = base64url_encode(&sig);
    format!("{signing_input}.{sig_b64}")
}

fn verify_jwt(token: &str, settings: &SettingsRepo) -> Result<String, String> {
    let secret = settings
        .get("jwt_secret")
        .ok()
        .flatten()
        .ok_or_else(|| "no jwt secret configured".to_string())?;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("invalid jwt format".into());
    }

    // Verify signature
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected_sig = hmac_sha256(signing_input.as_bytes(), secret.as_bytes());
    let actual_sig = base64url_decode(parts[2])?;
    if actual_sig != expected_sig {
        return Err("invalid signature".into());
    }

    // Decode payload and check expiry
    let payload_bytes = base64url_decode(parts[1])?;
    let payload_str =
        String::from_utf8(payload_bytes).map_err(|_| "invalid payload encoding".to_string())?;
    let payload: Value =
        serde_json::from_str(&payload_str).map_err(|_| "invalid payload json".to_string())?;

    let exp = payload["exp"].as_u64().unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now > exp {
        return Err("token expired".into());
    }

    payload["sub"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "missing sub claim".to_string())
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Check if auth is enabled
    let settings = SettingsRepo::new(state.db.clone());
    let auth_enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    if !auth_enabled {
        return next.run(request).await;
    }

    // Allow unauthenticated access to health, version, auth, and ws endpoints
    let path = request.uri().path();
    if path.contains("/system/health")
        || path.contains("/system/version")
        || path.contains("/auth/")
        || path == "/ws"
    {
        return next.run(request).await;
    }

    // Check for Authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    match auth_header {
        Some(header) if header.starts_with("Bearer ") => {
            let token = &header[7..];
            if verify_jwt(token, &settings).is_ok() {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "invalid token").into_response()
            }
        }
        Some(header) if header.starts_with("ApiKey ") => {
            let key = &header[7..];
            let stored = settings
                .get("api_key")
                .ok()
                .flatten()
                .unwrap_or_default();
            if !stored.is_empty() && key == stored {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
            }
        }
        _ => (StatusCode::UNAUTHORIZED, "authentication required").into_response(),
    }
}

// ---------------------------------------------------------------------------
// Auth routes
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/token", post(create_token))
        .route("/api-key", get(get_api_key).post(generate_api_key))
        .route("/config", get(auth_config).post(set_auth_config))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(State(state): State<AppState>, Json(body): Json<LoginRequest>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db.clone());

    // Verify against profile password_hash
    let conn = state.db.connection().lock().unwrap();
    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT password_hash FROM profiles WHERE username = ?",
            rusqlite::params![body.username],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    drop(conn);

    // Hash the provided password with SHA-256 for comparison
    let provided_hash = format!("{:x}", Sha256::digest(body.password.as_bytes()));

    let valid = match stored_hash {
        Some(ref h) if !h.is_empty() => *h == provided_hash,
        // If no password set, allow login for the default profile
        None | Some(_) => body.username == "default",
    };

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    // Ensure JWT secret exists
    let secret = match settings.get("jwt_secret").ok().flatten() {
        Some(s) if !s.is_empty() => s,
        _ => {
            let new_secret = uuid::Uuid::new_v4().to_string();
            settings.set("jwt_secret", &new_secret).ok();
            new_secret
        }
    };

    let token = create_jwt(&body.username, &secret);
    Json(json!({
        "token": token,
        "token_type": "Bearer",
        "expires_in": 86400,
        "username": body.username,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: Option<String>,
}

async fn create_token(
    State(state): State<AppState>,
    Json(body): Json<CreateTokenRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let name = body.name.as_deref().unwrap_or("api-token");

    let secret = match settings.get("jwt_secret").ok().flatten() {
        Some(s) if !s.is_empty() => s,
        _ => {
            let new_secret = uuid::Uuid::new_v4().to_string();
            settings.set("jwt_secret", &new_secret).ok();
            new_secret
        }
    };

    // Create a long-lived token (365 days)
    let header = base64url_encode(r#"{"alg":"HS256","typ":"JWT"}"#.as_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let payload_str = format!(
        r#"{{"sub":"{}","iat":{},"exp":{}}}"#,
        name,
        now,
        now + 365 * 86400
    );
    let payload = base64url_encode(payload_str.as_bytes());
    let signing_input = format!("{header}.{payload}");
    let sig = hmac_sha256(signing_input.as_bytes(), secret.as_bytes());
    let sig_b64 = base64url_encode(&sig);
    let token = format!("{signing_input}.{sig_b64}");

    Json(json!({
        "token": token,
        "name": name,
        "expires_in": 365 * 86400,
    }))
}

async fn get_api_key(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = settings.get("api_key").ok().flatten();
    let has_key = key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
    Json(json!({
        "has_key": has_key,
        "key_preview": key.as_ref().map(|k| {
            if k.len() > 8 { format!("{}...", &k[..8]) } else { k.clone() }
        }),
    }))
}

async fn generate_api_key(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let key = uuid::Uuid::new_v4().to_string().replace('-', "");
    settings.set("api_key", &key).ok();
    Json(json!({ "key": key }))
}

async fn auth_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let has_secret = settings
        .get("jwt_secret")
        .ok()
        .flatten()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_api_key = settings
        .get("api_key")
        .ok()
        .flatten()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Json(json!({
        "auth_enabled": enabled,
        "has_jwt_secret": has_secret,
        "has_api_key": has_api_key,
    }))
}

#[derive(Deserialize)]
struct SetAuthConfig {
    auth_enabled: Option<bool>,
    jwt_secret: Option<String>,
}

async fn set_auth_config(
    State(state): State<AppState>,
    Json(body): Json<SetAuthConfig>,
) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    if let Some(enabled) = body.auth_enabled {
        settings
            .set("auth_enabled", if enabled { "true" } else { "false" })
            .ok();

        // Auto-generate JWT secret when enabling auth
        if enabled {
            if settings
                .get("jwt_secret")
                .ok()
                .flatten()
                .map(|s| s.is_empty())
                .unwrap_or(true)
            {
                let secret = uuid::Uuid::new_v4().to_string();
                settings.set("jwt_secret", &secret).ok();
            }
        }
    }
    if let Some(ref secret) = body.jwt_secret {
        if !secret.is_empty() {
            settings.set("jwt_secret", secret).ok();
        }
    }

    let enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({ "auth_enabled": enabled }))
}
