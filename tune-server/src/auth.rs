use axum::extract::{ConnectInfo, FromRequestParts, OptionalFromRequestParts, Request, State};
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
pub use tune_http_types::AuthUser;

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

    // Allow unauthenticated access to health, version, auth, and ws endpoints.
    //
    // These are *substring* matches, so any path segment an extension controls
    // could otherwise smuggle itself into the allowlist: a plugin named `auth`
    // (mounted at /ext/auth/…) or any plugin route containing `/auth/` would
    // read as public and skip the token check entirely. Plugin namespaces are
    // never public — settle that before the allowlist gets a chance to match.
    //
    // Both spellings are tested because this middleware layers the `api`
    // router, which is itself nested under /api/v1: whether the prefix is
    // still on the URI here depends on axum's nesting internals, and this
    // check must not silently stop working if that changes.
    let path = request.uri().path();
    let method = request.method().clone();
    let is_extension_route = path.starts_with("/ext/") || path.starts_with("/api/v1/ext/");

    // Public allowlist. Health/version/ws/sso stay open. For /auth/ we no
    // longer blanket-public the whole namespace (the old `contains("/auth/")`):
    // the privileged endpoints — /auth/token, /auth/api-key, POST /auth/config —
    // MUST require a valid admin token even when auth is enabled, otherwise
    // enabling auth is defeated (an anonymous caller could mint a year-long JWT,
    // rotate the API key, or turn auth back off). Only the genuinely-public
    // handshakes are listed; everything else falls through to the token check.
    if !is_extension_route
        && (path.contains("/system/health")
            || path.contains("/system/version")
            // NB : /system/profile (fiche support) n'est volontairement PAS
            // dans cette allowlist : la fiche expose music_dirs et l'IP LAN,
            // trop large pour un appel anonyme quand l'auth est activée. Elle
            // requiert donc un token — n'importe quel rôle, pas de RequireAdmin :
            // l'écran Support est consulté par un utilisateur authentifié,
            // jamais de façon anonyme.
            // /system/peer-info EST public, et il doit l'etre : c'est la
            // poignee de main entre deux serveurs Tune. Un serveur protege
            // etait jusqu'ici IMPOSSIBLE a ajouter comme pair — l'autre bout
            // appelle cette route sans jeton (fetch_peer_info, admin.rs), et
            // il n'a aucun moyen d'en obtenir un.
            //
            // Contrairement a /system/profile juste au-dessus, la surface est
            // etroite et delibere : nom choisi par l'utilisateur, version,
            // nombre de pistes, nombre de zones. Ni music_dirs, ni IP, ni
            // chemin de fichier, ni reglage. C'est ce qu'un serveur affiche
            // deja de lui-meme dans la liste « serveurs du reseau » d'en face.
            //
            // GET seulement : la route est en lecture, et un POST homonyme
            // futur ne doit pas heriter de cette ouverture.
            || (method == axum::http::Method::GET
                && (path == "/system/peer-info" || path == "/api/v1/system/peer-info"))
            || path.contains("/cloud/sso/")
            || path == "/ws"
            || is_public_auth_route(&method, path))
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

/// The only /auth/ endpoints reachable without a token when auth is enabled:
/// login, logout, register (which can only create non-admin users), and the
/// read-only config *status* (GET /auth/config). Everything else in the auth
/// router — token minting, API-key generation/read, and mutating the auth
/// config — must carry a valid token so the handler can enforce the admin role.
fn is_public_auth_route(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    path.ends_with("/auth/login")
        || path.ends_with("/auth/logout")
        || path.ends_with("/auth/register")
        || (method == Method::GET && path.ends_with("/auth/config"))
}

/// When auth is enabled, require the caller to present a valid admin token.
/// When auth is disabled the whole API is open anyway (the middleware
/// short-circuits before any token check), so we must NOT 401 here — doing so
/// would break first-run setup, where the admin UI enables auth from an
/// unauthenticated, secret-less state.
fn ensure_admin_if_auth_enabled(
    settings: &SettingsRepo,
    user: Option<&AuthUser>,
) -> Result<(), Response> {
    let auth_enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    if !auth_enabled {
        return Ok(());
    }
    match user {
        Some(u) if u.role == "admin" => Ok(()),
        Some(_) => Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "admin role required"})),
        )
            .into_response()),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "authentication required"})),
        )
            .into_response()),
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

/// Authorize a WebSocket upgrade.
///
/// WS routers are mounted at the top level, *outside* the API auth middleware,
/// so a connection otherwise receives the full snapshot (zones, queue,
/// now-playing) and the live event stream with no token at all. This gates the
/// upgrade instead.
///
/// When auth is disabled the socket is open (unchanged behaviour). When enabled,
/// the caller must present a valid credential via:
/// - the `tune_session` cookie (browsers send it on same-origin handshakes, so
///   the logged-in web client keeps working),
/// - an `Authorization: Bearer <jwt>` / `ApiKey <key>` header (native clients),
/// - or a `?token=`/`?access_token=` query parameter (browsers cannot set
///   headers on a WebSocket handshake).
pub fn ws_authorized(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> bool {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let auth_enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    if !auth_enabled {
        return true;
    }

    let jwt_secret = settings
        .get("jwt_secret")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());

    // Header- or cookie-borne credential (Bearer JWT or ApiKey).
    if let Some(tok) = extract_token_from_headers(headers) {
        if let Some(key) = tok.strip_prefix("ApiKey:") {
            let stored = settings.get("api_key").ok().flatten().unwrap_or_default();
            if !stored.is_empty() && key == stored {
                return true;
            }
        } else if let Some(secret) = &jwt_secret {
            if verify_jwt(&tok, secret).is_ok() {
                return true;
            }
        }
    }

    // Query-param JWT (browser WebSocket can't set an Authorization header).
    if let (Some(tok), Some(secret)) = (query_token, &jwt_secret) {
        if !tok.is_empty() && verify_jwt(tok, secret).is_ok() {
            return true;
        }
    }

    false
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

/// axum 0.8 routes `Option<AuthUser>` through `OptionalFromRequestParts`, not
/// `FromRequestParts` — without this impl every `Option<AuthUser>` handler fails
/// the `Handler` bound. A missing or invalid credential is simply "no user"
/// (`Ok(None)`) for optional routes, which then decide access themselves (e.g.
/// `ensure_admin_if_auth_enabled`); it must NOT surface the strict extractor's
/// 401 rejection.
impl OptionalFromRequestParts<AppState> for AuthUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(
            <AuthUser as FromRequestParts<AppState>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

/// Extractor that requires an **admin** caller for privileged operations
/// (library wipe, config mutation, backup restore, DB export, restart,
/// self-update, …). The JWT already carries a role, but no business route
/// checked it — any valid token could hit these. This closes that (audit
/// item 4, RBAC).
///
/// When auth is disabled the server is fully open (first-run / trusted LAN by
/// choice), so this passes through — consistent with the rest of the auth
/// model, which only enforces once auth is enabled. When enabled, a non-admin
/// token gets 403 and a missing/invalid token 401.
pub struct RequireAdmin;

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let auth_enabled = settings
            .get("auth_enabled")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);
        if !auth_enabled {
            return Ok(RequireAdmin);
        }
        // Disambiguate: AuthUser now implements both FromRequestParts and
        // OptionalFromRequestParts (the latter merged in via the WS/auth work),
        // so the bare `AuthUser::from_request_parts` call is ambiguous (E0034).
        match <AuthUser as FromRequestParts<AppState>>::from_request_parts(parts, state).await {
            Ok(user) if user.role == "admin" => Ok(RequireAdmin),
            Ok(_) => Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "admin role required"})),
            )),
            Err(rejection) => Err(rejection),
        }
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
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    let username = body.username.trim().to_string();
    if username.is_empty() || body.password.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username required and password must be at least 8 characters"})),
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

    // Create profile. Atomically return the new id: the old code read
    // `last_insert_rowid()` BEFORE checking the INSERT even succeeded (and via a
    // separate lock, so a concurrent insert could hand back the wrong id) —
    // audit item 5.
    use tune_core::db::backend::ToSqlValue;
    let profile_id = match state.backend.execute_returning_id(
        "INSERT INTO profiles (username, display_name, password_hash_v2, email) VALUES (?, ?, ?, ?)",
        &[&username as &dyn ToSqlValue, &username as &dyn ToSqlValue, &password_hash as &dyn ToSqlValue, &body.email as &dyn ToSqlValue],
    ) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to create profile: {e}")})),
            )
                .into_response();
        }
    };

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

    let cookie = session_cookie(&token, &headers);

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

// ---------------------------------------------------------------------------
// Login brute-force throttling + cookie hardening
// ---------------------------------------------------------------------------

const LOGIN_MAX_FAILURES: u32 = 10;
const LOGIN_WINDOW: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Per-IP failed-login counters. The audit flagged that login had no attempt
/// throttling at all — this bounds online password guessing.
static LOGIN_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn login_rate_limited(ip: std::net::IpAddr) -> bool {
    // The local operator is never locked out.
    if ip.is_loopback() {
        return false;
    }
    let mut map = LOGIN_FAILURES.lock().unwrap();
    match map.get(&ip) {
        Some((count, first)) if first.elapsed() < LOGIN_WINDOW => *count >= LOGIN_MAX_FAILURES,
        Some(_) => {
            map.remove(&ip);
            false
        }
        None => false,
    }
}

fn record_login_failure(ip: std::net::IpAddr) {
    if ip.is_loopback() {
        return;
    }
    let mut map = LOGIN_FAILURES.lock().unwrap();
    let entry = map.entry(ip).or_insert((0, std::time::Instant::now()));
    if entry.1.elapsed() >= LOGIN_WINDOW {
        *entry = (1, std::time::Instant::now());
    } else {
        entry.0 += 1;
    }
}

fn clear_login_failures(ip: std::net::IpAddr) {
    LOGIN_FAILURES.lock().unwrap().remove(&ip);
}

/// Build the session cookie. `Secure` is added only when the request arrived
/// over HTTPS (via the `X-Forwarded-Proto` a TLS-terminating proxy sets), so
/// the cookie still works on a plain-HTTP LAN deployment while getting the
/// `Secure` flag wherever TLS is actually in play.
fn session_cookie(token: &str, headers: &axum::http::HeaderMap) -> String {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    let base = format!("tune_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400");
    if https {
        format!("{base}; Secure")
    } else {
        base
    }
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    if login_rate_limited(peer.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "too many login attempts, try again later"})),
        )
            .into_response();
    }
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
            record_login_failure(peer.ip());
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid credentials"})),
            )
                .into_response();
        }
    };

    // A `default` profile with no password hash is claimable on first login
    // (the provided password is persisted as the hash below — trust-on-first-use).
    // That must never be reachable from the network once auth is enabled, or an
    // attacker could seize the admin account before the operator sets a password.
    // Allow it only when auth is disabled (the server is fully open anyway) or the
    // request comes from loopback (the local operator performing first setup).
    let auth_enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let passwordless_default_allowed = !auth_enabled || peer.ip().is_loopback();

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
            // No password set — allow login for the default profile (loopback/first-run only)
            body.username == "default" && passwordless_default_allowed
        }
    } else {
        // No password set — allow login for the default profile (loopback/first-run only)
        body.username == "default" && passwordless_default_allowed
    };

    if !valid {
        record_login_failure(peer.ip());
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }
    clear_login_failures(peer.ip());

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

    let cookie = session_cookie(&token, &headers);

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
    user: Option<AuthUser>,
    Json(body): Json<CreateTokenRequest>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, user.as_ref()) {
        return resp;
    }
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

async fn get_api_key(State(state): State<AppState>, user: Option<AuthUser>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, user.as_ref()) {
        return resp;
    }
    let key = settings.get("api_key").ok().flatten();
    let has_key = key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
    Json(json!({
        "has_key": has_key,
        "key_preview": key.as_ref().map(|k| {
            if k.len() > 8 { format!("{}...", &k[..8]) } else { k.clone() }
        }),
    }))
    .into_response()
}

async fn generate_api_key(State(state): State<AppState>, user: Option<AuthUser>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, user.as_ref()) {
        return resp;
    }
    let key = uuid::Uuid::new_v4().to_string().replace('-', "");
    settings.set("api_key", &key).ok();
    Json(json!({ "key": key })).into_response()
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
    user: Option<AuthUser>,
    Json(body): Json<SetAuthConfig>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, user.as_ref()) {
        return resp;
    }
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
    Json(json!({ "auth_enabled": enabled })).into_response()
}

#[cfg(test)]
mod tests {
    use super::is_public_auth_route;
    use axum::http::Method;

    #[test]
    fn only_handshake_auth_routes_are_public() {
        // Public: login, logout, register, and the read-only config status (GET).
        assert!(is_public_auth_route(&Method::POST, "/api/v1/auth/login"));
        assert!(is_public_auth_route(&Method::POST, "/api/v1/auth/logout"));
        assert!(is_public_auth_route(&Method::POST, "/api/v1/auth/register"));
        assert!(is_public_auth_route(&Method::GET, "/api/v1/auth/config"));
        // Nesting may strip the /api/v1 prefix — both spellings must hold.
        assert!(is_public_auth_route(&Method::POST, "/auth/login"));
    }

    #[test]
    fn privileged_auth_routes_are_never_public() {
        // The P0: these must never be reachable without a token.
        assert!(!is_public_auth_route(&Method::POST, "/api/v1/auth/config"));
        assert!(!is_public_auth_route(&Method::POST, "/api/v1/auth/token"));
        assert!(!is_public_auth_route(&Method::GET, "/api/v1/auth/token"));
        assert!(!is_public_auth_route(&Method::POST, "/api/v1/auth/api-key"));
        assert!(!is_public_auth_route(&Method::GET, "/api/v1/auth/api-key"));
        assert!(!is_public_auth_route(&Method::GET, "/api/v1/auth/me"));
        assert!(!is_public_auth_route(
            &Method::POST,
            "/api/v1/system/audio/asio-warm-scan/rearm"
        ));
    }

    #[test]
    fn unrelated_paths_are_not_matched() {
        assert!(!is_public_auth_route(
            &Method::GET,
            "/api/v1/library/albums"
        ));
        assert!(!is_public_auth_route(
            &Method::POST,
            "/api/v1/system/restart"
        ));
    }
}
