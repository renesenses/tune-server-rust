use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// JWT Claims & AuthUser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject: user_id (profile id) as string
    pub sub: String,
    /// Issued at (unix timestamp)
    pub iat: u64,
    /// Expiration (unix timestamp)
    pub exp: u64,
    /// Role: "admin" or "user"
    pub role: String,
}

/// Injected into request extensions by the auth middleware / extractor.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: i64,
    pub role: String,
}

// ---------------------------------------------------------------------------
// JWT helpers
// ---------------------------------------------------------------------------

pub fn sign_jwt(user_id: i64, role: &str, secret: &str) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = JwtClaims {
        sub: user_id.to_string(),
        iat: now,
        exp: now + 86400, // 24h
        role: role.to_string(),
    };

    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| format!("jwt encode error: {e}"))
}

fn sign_jwt_long_lived(name: &str, secret: &str) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let claims = JwtClaims {
        sub: name.to_string(),
        iat: now,
        exp: now + 365 * 86400,
        role: "api-token".to_string(),
    };

    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| format!("jwt encode error: {e}"))
}

pub fn verify_jwt(token: &str, secret: &str) -> Result<JwtClaims, String> {
    jsonwebtoken::decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|e| format!("jwt verify error: {e}"))
}

// ---------------------------------------------------------------------------
// Argon2 password hashing
// ---------------------------------------------------------------------------

/// Generate a random salt string using system randomness.
fn generate_salt() -> SaltString {
    // Build 16 random bytes from multiple sources of entropy
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        let s = RandomState::new();
        let mut h = s.build_hasher();
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
        );
        let val = h.finish().to_le_bytes();
        let len = chunk.len().min(8);
        chunk[..len].copy_from_slice(&val[..len]);
    }

    // SaltString requires base64ct-encoded data; use b64 encoding of our random bytes
    SaltString::encode_b64(&bytes).expect("salt encoding")
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = generate_salt();
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("argon2 hash error: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ---------------------------------------------------------------------------
// JWT secret management
// ---------------------------------------------------------------------------

fn get_or_create_jwt_secret(settings: &SettingsRepo) -> String {
    match settings.get("jwt_secret").ok().flatten() {
        Some(s) if !s.is_empty() => s,
        _ => {
            let new_secret = uuid::Uuid::new_v4().to_string();
            settings.set("jwt_secret", &new_secret).ok();
            new_secret
        }
    }
}

// ---------------------------------------------------------------------------
// Middleware — auth layer applied to the API router
// ---------------------------------------------------------------------------

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
        || path.contains("/cloud/sso/")
        || path == "/ws"
    {
        return next.run(request).await;
    }

    // Extract token from Authorization header or tune_session cookie
    let token = extract_token_from_request(&request);

    match token {
        Some(tok) if tok.starts_with("ApiKey:") => {
            // API key auth
            let key = &tok[7..];
            let stored = settings.get("api_key").ok().flatten().unwrap_or_default();
            if !stored.is_empty() && key == stored {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
            }
        }
        Some(tok) => {
            let secret = match settings.get("jwt_secret").ok().flatten() {
                Some(s) if !s.is_empty() => s,
                _ => return (StatusCode::UNAUTHORIZED, "no jwt secret configured").into_response(),
            };
            match verify_jwt(&tok, &secret) {
                Ok(claims) => {
                    let user_id = claims.sub.parse::<i64>().unwrap_or(0);
                    request.extensions_mut().insert(AuthUser {
                        user_id,
                        role: claims.role,
                    });
                    next.run(request).await
                }
                Err(_) => (StatusCode::UNAUTHORIZED, "invalid token").into_response(),
            }
        }
        None => (StatusCode::UNAUTHORIZED, "authentication required").into_response(),
    }
}

/// Extract JWT token from Authorization header (Bearer) or tune_session cookie.
/// Returns "ApiKey:<key>" for API key auth.
fn extract_token_from_request(request: &Request) -> Option<String> {
    extract_token_from_headers(request.headers())
}

fn extract_token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    // 1. Check Authorization header
    if let Some(auth_header) = headers.get("Authorization").and_then(|v| v.to_str().ok()) {
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            return Some(token.to_string());
        }
        if let Some(key) = auth_header.strip_prefix("ApiKey ") {
            return Some(format!("ApiKey:{key}"));
        }
    }

    // 2. Check tune_session cookie
    if let Some(cookie_header) = headers.get("Cookie").and_then(|v| v.to_str().ok()) {
        for part in cookie_header.split(';') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("tune_session=") {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// AuthUser — axum extractor for route-level auth
// ---------------------------------------------------------------------------

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // First check if auth_middleware already injected AuthUser
        if let Some(user) = parts.extensions.get::<AuthUser>() {
            return Ok(user.clone());
        }

        // Otherwise, try to extract directly (for routes outside the middleware layer)
        let settings = SettingsRepo::with_backend(state.backend.clone());

        let secret = settings
            .get("jwt_secret")
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "authentication not configured"})),
                )
            })?;

        let token = extract_token_from_headers(&parts.headers).ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "authentication required"})),
            )
        })?;

        // Skip ApiKey tokens for the extractor
        if token.starts_with("ApiKey:") {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "API key auth not supported for this endpoint, use JWT"})),
            ));
        }

        let claims = verify_jwt(&token, &secret).map_err(|_| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid or expired token"})),
            )
        })?;

        let user_id = claims.sub.parse::<i64>().unwrap_or(0);
        Ok(AuthUser {
            user_id,
            role: claims.role,
        })
    }
}

// ---------------------------------------------------------------------------
// Auth routes
// ---------------------------------------------------------------------------

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/token", post(create_token))
        .route("/api-key", get(get_api_key).post(generate_api_key))
        .route("/config", get(auth_config).post(set_auth_config))
}

// ---------------------------------------------------------------------------
// POST /auth/register
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
    email: Option<String>,
}

async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    let username = body.username.trim().to_string();
    if username.is_empty() || body.password.len() < 4 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required and password must be at least 4 characters"})),
        )
            .into_response();
    }

    // Check if username already exists
    let exists = state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM profiles WHERE username = ?",
            &[&username as &dyn tune_core::db::backend::ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0;

    if exists {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "username already exists"})),
        )
            .into_response();
    }

    // Hash password with argon2
    let password_hash = match hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("password hashing failed: {e}")})),
            )
                .into_response();
        }
    };

    // Create profile
    use tune_core::db::backend::ToSqlValue;
    let result = state.backend.execute(
        "INSERT INTO profiles (username, display_name, password_hash_v2, email) VALUES (?, ?, ?, ?)",
        &[&username as &dyn ToSqlValue, &username as &dyn ToSqlValue, &password_hash as &dyn ToSqlValue, &body.email as &dyn ToSqlValue],
    );
    let profile_id = state.backend.last_insert_rowid();

    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("failed to create profile: {e}")})),
        )
            .into_response();
    }

    // Determine role
    let is_admin = profile_id == 1; // first user is admin
    let role = if is_admin { "admin" } else { "user" };

    // Generate JWT
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let secret = get_or_create_jwt_secret(&settings);
    let token = match sign_jwt(profile_id, role, &secret) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("token generation failed: {e}")})),
            )
                .into_response();
        }
    };

    let cookie = format!("tune_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400");

    let mut response = Json(json!({
        "token": token,
        "token_type": "Bearer",
        "expires_in": 86400,
        "user": {
            "id": profile_id,
            "username": username,
            "role": role,
            "email": body.email,
        }
    }))
    .into_response();

    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());

    (StatusCode::CREATED, response).into_response()
}

// ---------------------------------------------------------------------------
// POST /auth/login
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(State(state): State<AppState>, Json(body): Json<LoginRequest>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Look up profile
    use tune_core::db::backend::ToSqlValue;
    let row: Option<(i64, Option<String>, Option<String>, bool)> = state
        .backend
        .query_one(
            "SELECT id, password_hash, password_hash_v2, is_admin FROM profiles WHERE username = ?",
            &[&body.username as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .map(|r| {
            (
                r.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                r.get(1).and_then(|v| v.as_string()),
                r.get(2).and_then(|v| v.as_string()),
                r.get(3).and_then(|v| v.as_bool()).unwrap_or(false),
            )
        });

    let (profile_id, old_hash, new_hash, is_admin) = match row {
        Some(r) => r,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid credentials"})),
            )
                .into_response();
        }
    };

    // Try argon2 hash first (password_hash_v2), then fall back to SHA-256 (password_hash)
    let valid_v2 = if let Some(ref h) = new_hash {
        if !h.is_empty() {
            verify_password(&body.password, h)
        } else {
            false
        }
    } else {
        false
    };

    let valid = if valid_v2 {
        true
    } else if let Some(ref h) = old_hash {
        if !h.is_empty() {
            // Legacy SHA-256 check
            let provided_hash = format!("{:x}", Sha256::digest(body.password.as_bytes()));
            provided_hash == *h
        } else {
            // No password set — allow login for the default profile
            body.username == "default"
        }
    } else {
        // No password set — allow login for the default profile
        body.username == "default"
    };

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    // If logged in with old SHA-256 hash, upgrade to argon2
    if !valid_v2 && valid {
        if let Ok(upgraded) = hash_password(&body.password) {
            state
                .backend
                .execute(
                    "UPDATE profiles SET password_hash_v2 = ? WHERE id = ?",
                    &[&upgraded as &dyn ToSqlValue, &profile_id as &dyn ToSqlValue],
                )
                .ok();
        }
    }

    let role = if is_admin { "admin" } else { "user" };
    let secret = get_or_create_jwt_secret(&settings);
    let token = match sign_jwt(profile_id, role, &secret) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("token generation failed: {e}")})),
            )
                .into_response();
        }
    };

    let cookie = format!("tune_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400");

    let mut response = Json(json!({
        "token": token,
        "token_type": "Bearer",
        "expires_in": 86400,
        "username": body.username,
        "user_id": profile_id,
        "role": role,
    }))
    .into_response();

    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());

    response
}

// ---------------------------------------------------------------------------
// POST /auth/logout
// ---------------------------------------------------------------------------

async fn logout() -> impl IntoResponse {
    let cookie = "tune_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0";
    let mut response = Json(json!({"ok": true})).into_response();
    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    response
}

// ---------------------------------------------------------------------------
// GET /auth/me — requires auth
// ---------------------------------------------------------------------------

async fn me(State(state): State<AppState>, auth: AuthUser) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let row = state
        .backend
        .query_one(
            "SELECT id, username, display_name, avatar_path, is_admin, email, created_at FROM profiles WHERE id = ?",
            &[&auth.user_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten();

    match row {
        Some(r) => Json(json!({
            "id": r.get(0).and_then(|v| v.as_i64()),
            "username": r.get(1).and_then(|v| v.as_string()),
            "display_name": r.get(2).and_then(|v| v.as_string()),
            "avatar_path": r.get(3).and_then(|v| v.as_string()),
            "is_admin": r.get(4).and_then(|v| v.as_bool()),
            "email": r.get(5).and_then(|v| v.as_string()),
            "created_at": r.get(6).and_then(|v| v.as_string()),
            "role": auth.role,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /auth/token — create long-lived API token
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateTokenRequest {
    name: Option<String>,
}

async fn create_token(
    State(state): State<AppState>,
    Json(body): Json<CreateTokenRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let name = body.name.as_deref().unwrap_or("api-token");
    let secret = get_or_create_jwt_secret(&settings);

    match sign_jwt_long_lived(name, &secret) {
        Ok(token) => Json(json!({
            "token": token,
            "name": name,
            "expires_in": 365 * 86400,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// API key endpoints
// ---------------------------------------------------------------------------

async fn get_api_key(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = uuid::Uuid::new_v4().to_string().replace('-', "");
    settings.set("api_key", &key).ok();
    Json(json!({ "key": key }))
}

// ---------------------------------------------------------------------------
// Auth config endpoints
// ---------------------------------------------------------------------------

async fn auth_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
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
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(enabled) = body.auth_enabled {
        settings
            .set("auth_enabled", if enabled { "true" } else { "false" })
            .ok();

        // Auto-generate JWT secret when enabling auth
        if enabled {
            get_or_create_jwt_secret(&settings);
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
