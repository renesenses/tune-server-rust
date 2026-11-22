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
            // Seul le retour du SSO est public (A5) : chemin exact, GET. Le
            // navigateur y revient de mozaiklabs.fr ; `authorize`, `status`
            // et `disconnect` demandent une session (le cookie la porte).
            || (method == axum::http::Method::GET
                && (path == "/cloud/sso/callback" || path == "/api/v1/cloud/sso/callback"))
            || path == "/ws"
            // #8189 — la passerelle des sources réseau : un renderer n'a pas
            // de session. Le gestionnaire exige le jeton signé, propre au
            // fichier et de courte durée ; GET et HEAD seulement.
            || ((method == axum::http::Method::GET || method == axum::http::Method::HEAD)
                && crate::routes::sources_reseau::est_route_passerelle(path))
            || is_public_auth_route(&method, path))
    {
        return next.run(request).await;
    }

    // #3933 — les ressources que le serveur média publie dans sa DIDL.
    //
    // Un renderer DLNA n'a AUCUN moyen de s'authentifier : il n'y a ni écran
    // de connexion ni en-tête `Authorization` dans un Marantz. Or les quatre
    // URL que le serveur média lui donne (audio de piste, flux radio,
    // pochette, relais de logo) vivent toutes sous `/api/v1`, la seule surface
    // que cette couche protège. Avec `auth_enabled = true`, le renderer voyait
    // le serveur, le parcourait — et se faisait refuser en 401 tout ce qu'il y
    // trouvait. Le chemin PUSH marchait (`/stream/…` est monté à la racine),
    // le chemin PULL non : c'est l'asymétrie que les testeurs rapportaient.
    //
    // L'arbitrage retenu, et ses bornes :
    //
    // * la liste des chemins n'est PAS écrite ici — elle est rendue par
    //   `est_ressource_didl`, qui vit à côté des constructeurs d'URL. Deux
    //   listes séparées, c'est la dérive garantie le jour où une cinquième
    //   ressource entre dans la DIDL ;
    // * GET et HEAD seulement. Un renderer sonde en HEAD avant de lire — la
    //   route radio a d'ailleurs son propre gestionnaire `.head(…)`. Tout le
    //   reste retombe sur le contrôle de jeton ;
    // * appelant sur le réseau local uniquement. Un serveur publié sur
    //   Internet ne s'ouvre pas : la DIDL n'est de toute façon atteignable
    //   qu'en SSDP, donc depuis le LAN.
    //
    // Ce qui a été ÉCARTÉ : « n'exempter que ceux qui ont parcouru la DIDL ».
    // En DLNA le point de contrôle qui parcourt (le téléphone) et le renderer
    // qui va chercher les octets (l'ampli) sont deux machines différentes, avec
    // deux adresses différentes. Une exemption gagnée en parcourant n'aurait
    // jamais profité à celui qui en a besoin.
    if !is_extension_route
        && (method == axum::http::Method::GET || method == axum::http::Method::HEAD)
        && tune_core::upnp_server::est_ressource_didl(path)
        && appelant_sur_le_reseau_local(&request)
    {
        // #4260 : le gestionnaire du relais de pochettes doit savoir que
        // l'appel est entré SANS jeton, par cette exemption — il exige alors
        // l'URL signée que la DIDL publie, et rien d'autre.
        request.extensions_mut().insert(ExemptionDidl);
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

/// Marqueur posé sur la requête quand elle est entrée par l'exemption DIDL
/// (#3933) : sans jeton, depuis le LAN, sur une ressource que le serveur média
/// publie. Le relais de pochettes (`proxy_artwork`) le lit pour exiger une URL
/// signée (#4260) — un appelant authentifié, lui, n'a pas ce marqueur.
///
/// Posé UNIQUEMENT ici, quand l'authentification est active : avec
/// `auth_enabled = false`, rien ne distingue le client web d'un renderer, et
/// le relais retombe sur sa liste d'hôtes pour tout le monde.
#[derive(Debug, Clone, Copy)]
pub struct ExemptionDidl;

/// L'appelant est-il sur le réseau local (ou la machine elle-même) ?
///
/// L'adresse vient de `ConnectInfo`, posé par
/// `into_make_service_with_connect_info` (`bootstrap.rs`) et par
/// `MockConnectInfo` sous test. **Absente, la réponse est NON** : une couche
/// qui oublierait de le poser doit fermer l'exemption, pas l'ouvrir.
fn appelant_sur_le_reseau_local(request: &Request) -> bool {
    // Rejouée par le relais ou l'écoute directe : vue de 127.0.0.1, mais
    // émise ailleurs. Jamais locale.
    if crate::garde_du_relais::requete_relayee(request.headers()) {
        return false;
    }
    request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(peer)| {
            let ip = peer.ip();
            // Sécurité : sur une appliance dont le relais du port 80 rouvre
            // les connexions depuis 127.0.0.1, un pair en bouclage peut venir
            // de n'importe où — il ne vaut plus « réseau local ».
            if adresse_de_cette_machine(ip) && !crate::routes::appliance::bouclage_fiable() {
                return false;
            }
            adresse_locale(ip)
        })
        .unwrap_or(false)
}

/// Loopback, plages privées RFC 1918, lien local — et leurs équivalents IPv6.
///
/// Écrit à la main plutôt qu'avec `Ipv6Addr::is_unique_local` /
/// `is_unicast_link_local`, qui sont encore instables : les deux masques
/// ci-dessous sont ceux de la RFC (fc00::/7 et fe80::/10).
fn adresse_locale(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    fn v4_local(v4: &std::net::Ipv4Addr) -> bool {
        v4.is_loopback() || v4.is_private() || v4.is_link_local()
    }
    match ip {
        IpAddr::V4(v4) => v4_local(&v4),
        IpAddr::V6(v6) => {
            // Une socket à double pile rend l'appelant IPv4 sous la forme
            // ::ffff:192.168.1.42 — sans ce déballage, tout le LAN IPv4 d'un
            // serveur en écoute IPv6 serait vu comme distant.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4_local(&v4);
            }
            let s = v6.segments();
            v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// The only /auth/ endpoints reachable without a token when auth is enabled:
/// login, logout, and the read-only config *status* (GET /auth/config).
/// Everything else in the auth router — registration (only an
/// administrator creates accounts once auth is on), token minting, API-key
/// generation/read, and mutating the auth config — must carry a valid token so
/// the handler can enforce the admin role.
fn is_public_auth_route(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    path.ends_with("/auth/login")
        || path.ends_with("/auth/logout")
        || (method == Method::GET && path.ends_with("/auth/config"))
        // L'écran d'activation lit seulement l'état (activée ou non, appel de
        // cette machine ou non).
        || (method == Method::GET && path.ends_with("/auth/activation"))
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
            if let Some(value) = part.strip_prefix("tune_session=")
                && !value.is_empty()
            {
                return Some(value.to_string());
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
        } else if let Some(secret) = &jwt_secret
            && verify_jwt(&tok, secret).is_ok()
        {
            return true;
        }
    }

    // Query-param JWT (browser WebSocket can't set an Authorization header).
    if let (Some(tok), Some(secret)) = (query_token, &jwt_secret)
        && !tok.is_empty()
        && verify_jwt(tok, secret).is_ok()
    {
        return true;
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

/// L'authentification est-elle active sur ce serveur ?
pub fn authentification_active(state: &AppState) -> bool {
    authentification_active_db(&state.backend)
}

/// [`authentification_active`] sans état HTTP (planificateurs, #8909).
pub fn authentification_active_db(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get("auth_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Le jeton du pont (relais cloud, écoute à distance) change à l'activation :
/// un lien d'accès obtenu pendant que tout était ouvert ne sert plus, et la
/// session du relais est rouverte avec le nouveau jeton. Les apps d'écoute à
/// distance sont à appairer de nouveau.
fn renouveler_le_jeton_du_pont(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let present = settings
        .get("bridge_token")
        .ok()
        .flatten()
        .is_some_and(|t| !t.is_empty());
    if !present {
        return;
    }
    let jeton = uuid::Uuid::new_v4().to_string();
    if settings.set("bridge_token", &jeton).is_err() {
        tracing::warn!("activation_jeton_du_pont_non_enregistre");
        return;
    }
    #[cfg(feature = "cloud-relay")]
    if let Some(client) = state.relay_client.get() {
        client.renouveler_jeton(jeton);
    }
    #[cfg(not(feature = "cloud-relay"))]
    let _ = jeton;
    tracing::info!("activation_jeton_du_pont_renouvele");
}

/// Refus d'une opération réservée aux serveurs dont l'authentification est
/// active : `RequireAdmin` laisse tout passer quand elle est coupée.
pub fn refus_sans_authentification(message: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "auth_required",
            "message": message,
        })),
    )
}

/// Sécurité — l'adresse est-elle CETTE machine ? Bouclage IPv4
/// (`127.0.0.0/8`), IPv6 (`::1`) et la forme IPv4 d'une socket à double pile
/// (`::ffff:127.x`). Le réseau local n'en fait PAS partie.
pub fn adresse_de_cette_machine(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Sécurité — garde des routes qui LISENT ou REMPLACENT la base
/// entière (export, import, sauvegarde chiffrée, restauration).
///
/// Authentification active : rien de plus ici, `RequireAdmin` (à placer
/// AVANT cet extracteur) exige déjà un jeton administrateur. Authentification
/// coupée : `RequireAdmin` laisse passer tout le réseau, donc seule une
/// requête venue de cette machine passe. L'adresse est celle de la
/// CONNEXION (`ConnectInfo`, posé par `aiguillage_des_flux`), jamais un
/// en-tête ; absente, la réponse est non. Une requête rejouée par le relais
/// ou l'écoute directe (vue de 127.0.0.1, émise ailleurs) n'est jamais locale.
pub struct BaseEntiereDepuisCetteMachine;

impl FromRequestParts<AppState> for BaseEntiereDepuisCetteMachine {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if authentification_active(state) {
            return Ok(Self);
        }
        let relayee = crate::garde_du_relais::requete_relayee(&parts.headers);
        // Même lecture que `AdminOuCetteMachine` : adresse de la CONNEXION
        // par l'extracteur `ConnectInfo` (absente : non), jamais une requête
        // relayée, et pas de bouclage sur une appliance qui relaie le port 80.
        if appel_depuis_cette_machine(parts, state).await {
            return Ok(Self);
        }
        tracing::warn!(
            chemin = %parts.uri.path(),
            relayee,
            "base_entiere_refusee_hors_machine_sans_authentification"
        );
        // Une phrase lisible, dans la langue de l'interface
        // (`Accept-Language`, repli anglais), avec sa clé : le client web
        // l'affiche telle quelle (contrat de `crate::message_api`).
        let lang = crate::i18n::lang_from_header(&parts.headers);
        Err((
            StatusCode::FORBIDDEN,
            Json(crate::message_api::avec_message_en(
                json!({ "error": "auth_required", "code": "auth_required" }),
                &lang,
                "api.database.localOnly",
                &[],
            )),
        ))
    }
}

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

/// Garde des actions qui engagent la machine (effacer un disque, éteindre,
/// installer un greffon) : l'administrateur quand l'authentification est
/// active ; sinon, SEULEMENT un appel émis depuis cette machine (adresse de
/// bouclage réelle de la connexion, jamais une requête relayée). Même doctrine
/// que la garde de la base pour la migration : « cette machine », pas « le réseau local ».
///
/// `RequireAdmin` laisse tout passer quand l'authentification est coupée ;
/// cette garde ne le fait pas.
pub struct AdminOuCetteMachine;

/// L'appel vient-il de cette machine ? Lu dans `ConnectInfo` (posé par
/// `into_make_service_with_connect_info`, ou `MockConnectInfo` sous test).
/// **Absent, la réponse est NON.** Une requête relayée (vue de 127.0.0.1 mais
/// émise ailleurs) n'est jamais locale.
async fn appel_depuis_cette_machine(parts: &mut Parts, state: &AppState) -> bool {
    if crate::garde_du_relais::requete_relayee(&parts.headers) {
        return false;
    }
    // Derrière un mandataire inverse local (nginx, Caddy, proxy DSM,
    // Tailscale Serve, cloudflared…), tout appel arrive de 127.0.0.1 : le
    // bouclage ne prouve plus rien.
    // HTTP/1.0 : l'empreinte d'un mandataire inverse (nginx par défaut :
    // `proxy_http_version 1.0`, `Host` réécrit en 127.0.0.1, aucun
    // `X-Forwarded-*`). Un navigateur parle HTTP/1.1 ou 2.
    if parts.version == axum::http::Version::HTTP_10 {
        return false;
    }
    let autorite = parts.uri.authority().map(|a| a.as_str().to_string());
    if !requete_de_cette_machine(&parts.headers, autorite.as_deref()) {
        return false;
    }
    // Appliance + relais du port 80 : le bouclage ne prouve rien (voir
    // `appliance::bouclage_fiable`). Il faut alors un administrateur.
    if !crate::routes::appliance::bouclage_fiable() {
        return false;
    }
    adresse_de_l_appelant(parts, state)
        .await
        .is_some_and(adresse_de_cette_machine)
}

/// La même règle que [`AdminOuCetteMachine`], sans rejet ni trace : pour une
/// route ouverte à tous qui ne détaille certains champs qu'à l'administrateur
/// (authentification active) ou à un appel de cette machine (#9083).
pub async fn admin_ou_cette_machine(parts: &mut Parts, state: &AppState) -> bool {
    if authentification_active(state) {
        return <RequireAdmin as FromRequestParts<AppState>>::from_request_parts(parts, state)
            .await
            .is_ok();
    }
    appel_depuis_cette_machine(parts, state).await
}

/// Extracteur infaillible : `true` pour l'administrateur (authentification
/// active) ou un appel de cette machine (authentification coupée) — ceux qui
/// relient le serveur, et seuls à lire les comptes Mozaiklabs liés. Même règle
/// que [`admin_ou_cette_machine`], sans rejet ni trace : pour des routes de
/// lecture appelées en boucle par tous les appareils.
#[derive(Debug, Clone, Copy)]
pub struct ReliePourLesComptes(pub bool);

impl FromRequestParts<AppState> for ReliePourLesComptes {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(admin_ou_cette_machine(parts, state).await))
    }
}

/// En-têtes qu'un mandataire inverse ajoute en relayant une requête.
const ENTETES_DE_MANDATAIRE: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
    "x-client-ip",
    "via",
    "cf-connecting-ip",
    "true-client-ip",
    "fastly-client-ip",
    "tailscale-user-login",
];

/// Le nom d'hôte désigne-t-il cette machine (`localhost`, `127.x.x.x`,
/// `[::1]`, port éventuel) ?
pub fn hote_de_bouclage(hote: &str) -> bool {
    let hote = hote.trim().trim_end_matches('.').to_ascii_lowercase();
    // `[::1]:8888`, `[::1]`, `localhost:8888`, `127.0.0.1:8888`.
    let nom = if let Some(reste) = hote.strip_prefix('[') {
        // `[::1]` ou `[::1]:port` ; un crochet non refermé n'est rien.
        match reste.split_once(']') {
            Some((nom, apres)) if apres.is_empty() || apres.starts_with(':') => nom,
            _ => return false,
        }
    } else {
        hote.rsplit_once(':')
            .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
            .map_or(hote.as_str(), |(nom, _)| nom)
    };
    let nom = nom.trim_end_matches('.');
    nom == "localhost"
        || nom
            .parse::<std::net::IpAddr>()
            .is_ok_and(adresse_de_cette_machine)
}

/// La requête vient-elle de cette machine, côté en-têtes ? Aucun en-tête de
/// mandataire inverse, aucun mandataire déclaré, et un `Host` (ou l'autorité
/// de l'URI, HTTP/2) qui nomme le bouclage.
///
/// Sans `Host` ni autorité en HTTP/1.1 (client minimal), la réponse reste
/// oui : un navigateur en envoie toujours un. HTTP/1.0 est refusé plus haut
/// ([`appel_depuis_cette_machine`]). Couvert : nginx avec les en-têtes
/// `X-Forwarded-*` habituels, ou en HTTP/1.0 par défaut ; Caddy, Traefik,
/// proxy DSM, Tailscale Serve, cloudflared (en-têtes). NON distinguable :
/// HAProxy en mode TCP, `socat`, relais TCP qui ne posent aucun en-tête et
/// transmettent un `Host` de bouclage en HTTP/1.1 — déclarer alors
/// `TUNE_ALLOWED_HOSTS`, ce qui retire la preuve du bouclage (il faut
/// l'authentification).
pub fn requete_de_cette_machine(entetes: &axum::http::HeaderMap, autorite: Option<&str>) -> bool {
    if derriere_un_mandataire(entetes) {
        return false;
    }
    let hote = entetes
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or(autorite);
    hote.is_none_or(hote_de_bouclage)
}

/// La requête est-elle passée par un mandataire inverse, ou le serveur
/// est-il déclaré servi derrière l'un d'eux (`TUNE_ALLOWED_ORIGINS`,
/// `TUNE_ALLOWED_HOSTS`) ? Alors le bouclage n'est pas une preuve (même
/// principe que [`crate::routes::appliance::bouclage_fiable`]).
pub fn derriere_un_mandataire(entetes: &axum::http::HeaderMap) -> bool {
    ENTETES_DE_MANDATAIRE
        .iter()
        .any(|e| entetes.contains_key(*e))
        || mandataire_declare(
            std::env::var(crate::garde_origine::VARIABLE_ORIGINES)
                .ok()
                .as_deref(),
            std::env::var(crate::garde_origine::VARIABLE_HOTES)
                .ok()
                .as_deref(),
        )
}

/// Une des deux variables qui déclarent un mandataire est-elle renseignée ?
pub fn mandataire_declare(origines: Option<&str>, hotes: Option<&str>) -> bool {
    [origines, hotes]
        .into_iter()
        .flatten()
        .any(|v| v.split(',').any(|x| !x.trim().is_empty()))
}

/// L'adresse réelle de la connexion, si la couche de service l'a posée.
///
/// Par l'EXTRACTEUR `ConnectInfo`, pas par une lecture directe de l'extension :
/// sous test, `MockConnectInfo` pose sa propre extension et seul l'extracteur
/// sait la lire. Une lecture directe voit « rien » et refuse tout.
async fn adresse_de_l_appelant(parts: &mut Parts, state: &AppState) -> Option<std::net::IpAddr> {
    <ConnectInfo<std::net::SocketAddr> as FromRequestParts<AppState>>::from_request_parts(
        parts, state,
    )
    .await
    .ok()
    .map(|ConnectInfo(pair)| pair.ip())
}

impl FromRequestParts<AppState> for AdminOuCetteMachine {
    type Rejection = (StatusCode, Json<Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if authentification_active(state) {
            return <RequireAdmin as FromRequestParts<AppState>>::from_request_parts(parts, state)
                .await
                .map(|_| AdminOuCetteMachine);
        }
        if appel_depuis_cette_machine(parts, state).await {
            return Ok(AdminOuCetteMachine);
        }
        let appelant = adresse_de_l_appelant(parts, state)
            .await
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let relaye = crate::garde_du_relais::requete_relayee(&parts.headers);
        let relais_port_80 = !crate::routes::appliance::bouclage_fiable();
        let mandataire = derriere_un_mandataire(&parts.headers);
        tracing::warn!(
            chemin = %parts.uri.path(),
            %appelant,
            relaye,
            relais_port_80,
            mandataire,
            "action_machine_refusee_hors_de_cette_machine_sans_authentification"
        );
        // Le refus dit ce que le serveur a vu : l'exploitant comprend tout de
        // suite pourquoi « depuis cette machine » n'a pas été reconnu.
        let (motif, message) = if relais_port_80 {
            (
                "port80_relay",
                "This appliance relays port 80 to the server from 127.0.0.1, so a local \
                 call cannot be told apart from the network: enable authentication and \
                 sign in as an administrator (or rebuild the Tune OS image).",
            )
        } else {
            (
                "not_local",
                "This action requires authentication to be enabled, or a call from this \
                 machine (localhost).",
            )
        };
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "auth_required",
                "reason": motif,
                "message": message,
                "caller": appelant,
                "relayed": relaye,
            })),
        ))
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
        .route(
            "/activation",
            get(etat_activation).post(activer_l_authentification),
        )
        .route("/activation/code", post(demander_un_code_d_activation))
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
    DepuisCetteMachine(cette_machine): DepuisCetteMachine,
    appelant: Option<AuthUser>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    // Sécurité : l'inscription libre n'existe que tant que l'authentification
    // est coupée (premier compte, à l'onboarding). Une fois active, seul un
    // administrateur crée un compte — sinon n'importe qui sur le réseau
    // s'ouvrait une session « user ».
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, appelant.as_ref()) {
        return resp;
    }
    // Authentification coupée : un compte créé par un tiers du réseau
    // survivrait à l'activation. Seulement depuis cette machine (mêmes
    // règles que `AdminOuCetteMachine`, mandataire inverse compris).
    if !authentification_active(&state) && !cette_machine {
        tracing::warn!("inscription_refusee_hors_machine_sans_authentification");
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "auth_required",
                "reason": "not_local",
                "message": "While authentication is disabled, accounts can only be created from this machine (localhost). Enable authentication first.",
            })),
        )
            .into_response();
    }
    let cree_par_un_administrateur = appelant.is_some();
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

    let utilisateur = json!({
        "id": profile_id,
        "username": username,
        "role": role,
        "email": body.email,
    });
    // Un administrateur qui crée un compte ne reçoit NI le jeton du nouveau
    // compte NI son cookie : il garde sa session, l'autre se connectera.
    let mut response = if cree_par_un_administrateur {
        Json(json!({ "user": utilisateur })).into_response()
    } else {
        Json(json!({
            "token": token,
            "token_type": "Bearer",
            "expires_in": 86400,
            "user": utilisateur,
        }))
        .into_response()
    };

    if !cree_par_un_administrateur {
        let cookie = session_cookie(&token, &headers);
        response
            .headers_mut()
            .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    }

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

/// Le bouclage que l'on croit : l'opérateur local, et seulement lui
/// (Sécurité : sur une appliance qui relaie le port 80 depuis 127.0.0.1, un
/// pair en bouclage peut être n'importe qui du réseau).
fn bouclage_de_l_operateur(ip: std::net::IpAddr) -> bool {
    adresse_de_cette_machine(ip) && crate::routes::appliance::bouclage_fiable()
}

fn login_rate_limited(ip: std::net::IpAddr, operateur: bool) -> bool {
    // The local operator is never locked out (never behind a reverse proxy:
    // every proxied client would share its loopback address).
    if operateur {
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

fn record_login_failure(ip: std::net::IpAddr, operateur: bool) {
    if operateur {
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
    version: axum::http::Version,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // Une requête relayée est comptée sous une adresse qui n'est jamais en
    // boucle locale : la limite de tentatives s'applique.
    let ip = crate::garde_du_relais::adresse_de_l_appelant(peer.ip(), &headers);
    // L'opérateur de CETTE machine : bouclage réel, ni mandataire inverse ni
    // `Host` d'un autre nom.
    let operateur = bouclage_de_l_operateur(ip)
        && version != axum::http::Version::HTTP_10
        && requete_de_cette_machine(&headers, None);
    if login_rate_limited(ip, operateur) {
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
                r.first().and_then(|v| v.as_i64()).unwrap_or(0),
                r.get(1).and_then(|v| v.as_string()),
                r.get(2).and_then(|v| v.as_string()),
                r.get(3).and_then(|v| v.as_bool()).unwrap_or(false),
            )
        });

    let (profile_id, old_hash, new_hash, is_admin) = match row {
        Some(r) => r,
        None => {
            record_login_failure(ip, operateur);
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
    let passwordless_default_allowed = !auth_enabled || operateur;

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
        record_login_failure(ip, operateur);
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }
    clear_login_failures(ip);

    // If logged in with old SHA-256 hash, upgrade to argon2. Le profil
    // `default` sans mot de passe n'est réclamé (mot de passe enregistré) que
    // par l'opérateur de cette machine : depuis le réseau, authentification
    // coupée, la session est ouverte mais le compte administrateur n'est pas
    // pris.
    let ancien_hachage = old_hash.as_deref().is_some_and(|h| !h.is_empty());
    if !valid_v2
        && valid
        && (ancien_hachage || operateur)
        && let Ok(upgraded) = hash_password(&body.password)
    {
        state
            .backend
            .execute(
                "UPDATE profiles SET password_hash_v2 = ? WHERE id = ?",
                &[&upgraded as &dyn ToSqlValue, &profile_id as &dyn ToSqlValue],
            )
            .ok();
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
            "id": r.first().and_then(|v| v.as_i64()),
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
    DepuisCetteMachine(cette_machine): DepuisCetteMachine,
    user: Option<AuthUser>,
    Json(body): Json<SetAuthConfig>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(resp) = ensure_admin_if_auth_enabled(&settings, user.as_ref()) {
        return resp;
    }
    let etait_active = authentification_active(&state);
    // Authentification coupée, activer ou poser le secret de signature décide
    // qui contrôlera le serveur : seulement depuis cette machine. Depuis un
    // autre appareil, c'est `POST /auth/activation` avec le code du journal.
    let veut_activer = body.auth_enabled == Some(true);
    let pose_un_secret = body.jwt_secret.as_deref().is_some_and(|s| !s.is_empty());
    if !etait_active && (veut_activer || pose_un_secret) && !cette_machine {
        tracing::warn!("activation_authentification_refusee_hors_machine");
        return refus_activation_hors_machine();
    }
    if let Some(enabled) = body.auth_enabled {
        settings
            .set("auth_enabled", if enabled { "true" } else { "false" })
            .ok();

        if enabled && !etait_active {
            // Les jetons et la clé émis pendant que tout était ouvert ne
            // survivent pas à l'activation.
            renouveler_les_secrets(&settings);
            renouveler_le_jeton_du_pont(&state);
            state.fermeture_websockets.send_modify(|n| *n += 1);
        } else if enabled {
            get_or_create_jwt_secret(&settings);
        }
    }
    if let Some(ref secret) = body.jwt_secret
        && !secret.is_empty()
    {
        settings.set("jwt_secret", secret).ok();
    }

    let enabled = settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({ "auth_enabled": enabled })).into_response()
}

// ---------------------------------------------------------------------------
// Activation de l'authentification depuis l'interface
// ---------------------------------------------------------------------------

/// L'appel vient-il de cette machine ? Même lecture que
/// [`AdminOuCetteMachine`] (adresse réelle de la connexion, jamais relayée),
/// sans refus : le gestionnaire décide.
pub struct DepuisCetteMachine(pub bool);

impl FromRequestParts<AppState> for DepuisCetteMachine {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(appel_depuis_cette_machine(parts, state).await))
    }
}

/// Code d'activation à usage unique, écrit sur la CONSOLE du serveur
/// seulement (voir [`emettre_code_activation`]).
pub struct CodeActivation {
    code: String,
    emis: std::time::Instant,
    essais_rates: u32,
}

/// L'état du code d'activation : le code en cours, la dernière émission et les
/// échecs comptés par adresse.
#[derive(Default)]
pub struct EtatCodeActivation {
    en_cours: Option<CodeActivation>,
    derniere_emission: Option<std::time::Instant>,
    echecs_par_ip: std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>,
}

/// Durée de vie d'un code d'activation.
pub const DUREE_CODE_ACTIVATION: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// Essais faux tolérés avant que le code soit détruit.
pub const ESSAIS_CODE_ACTIVATION: u32 = 5;
/// Au plus une émission par minute.
pub const DELAI_ENTRE_EMISSIONS: std::time::Duration = std::time::Duration::from_secs(60);
/// Essais faux tolérés par adresse, tous codes confondus, sur une heure.
pub const ECHECS_PAR_ADRESSE: u32 = 8;
const FENETRE_ECHECS: std::time::Duration = std::time::Duration::from_secs(3600);
/// Longueur du code (31 symboles : environ 50 bits).
const LONGUEUR_CODE: usize = 10;

/// Lettres et chiffres sans ambiguïté de lecture (ni 0/O, ni 1/I/L).
const ALPHABET_CODE: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Tirage SANS BIAIS : octets du générateur du système (`uuid` v4 lit
/// `getrandom`), en écartant les deux octets que la v4 fixe en partie
/// (version, variante) et, par rejet, les valeurs au-delà du plus grand
/// multiple de 31.
fn nouveau_code_activation() -> String {
    let limite = (256 / ALPHABET_CODE.len() * ALPHABET_CODE.len()) as u8;
    let mut code = String::with_capacity(LONGUEUR_CODE);
    while code.len() < LONGUEUR_CODE {
        let octets = *uuid::Uuid::new_v4().as_bytes();
        for (i, o) in octets.iter().enumerate() {
            if i == 6 || i == 8 || *o >= limite {
                continue;
            }
            code.push(ALPHABET_CODE[*o as usize % ALPHABET_CODE.len()] as char);
            if code.len() == LONGUEUR_CODE {
                break;
            }
        }
    }
    code
}

/// Normalise une saisie : majuscules, sans espaces ni tirets.
fn normaliser_code(saisi: &str) -> String {
    saisi
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

/// Ce que donne une demande de code.
#[derive(Debug, PartialEq, Eq)]
pub enum EmissionCode {
    /// Un nouveau code est sur la console.
    Emis,
    /// Un code encore valable est déjà en cours : il n'est pas remplacé et
    /// ses essais ne sont pas remis à zéro.
    DejaEnCours,
    /// Moins d'une minute depuis la dernière émission.
    TropTot,
}

fn code_valable(c: &CodeActivation) -> bool {
    c.emis.elapsed() <= DUREE_CODE_ACTIVATION && c.essais_rates < ESSAIS_CODE_ACTIVATION
}

/// Émet un code, s'il n'y en a pas déjà un de valable, et l'écrit sur la
/// CONSOLE du serveur (sortie d'erreur : `docker logs`, `journalctl -u tune`,
/// en SSH sur Tune OS ; sous Windows sans console, le fichier journal) — jamais dans le journal de `tracing`, que l'API sert
/// (`/system/logs`, rapports de bogue). La ligne porte
/// [`tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION`], que tous les
/// lecteurs de journaux retirent (cas d'une sortie d'erreur redirigée dans le
/// fichier journal).
pub fn emettre_code_activation(state: &AppState) -> EmissionCode {
    let mut etat = state.code_activation_auth.lock().unwrap();
    if etat.en_cours.as_ref().is_some_and(code_valable) {
        return EmissionCode::DejaEnCours;
    }
    if etat
        .derniere_emission
        .is_some_and(|t| t.elapsed() < DELAI_ENTRE_EMISSIONS)
    {
        return EmissionCode::TropTot;
    }
    let code = nouveau_code_activation();
    etat.en_cours = Some(CodeActivation {
        code: code.clone(),
        emis: std::time::Instant::now(),
        essais_rates: 0,
    });
    etat.derniere_emission = Some(std::time::Instant::now());
    drop(etat);
    eprintln!(
        "{} {code} — Tune : code d'activation de l'authentification, usage unique, {} min. \
         Saisissez-le dans Réglages > Système > Authentification. / One-time code to enable \
         authentication in Settings > System > Authentication.",
        tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION,
        DUREE_CODE_ACTIVATION.as_secs() / 60,
    );
    tracing::warn!("auth_activation_code_ecrit_sur_la_console");
    // Windows sans console (`lancement_sans_console`) : la sortie d'erreur
    // ne va nulle part. Le code part aussi dans le fichier journal, que seul
    // l'exploitant de la machine lit ; tous les lecteurs de l'API retirent
    // la ligne (marqueur).
    #[cfg(windows)]
    tracing::warn!(
        "{} {} (Windows : fichier journal tune-server.log)",
        tune_core::confidentialite::MARQUEUR_CODE_ACTIVATION,
        code,
    );
    EmissionCode::Emis
}

/// Le code en cours, comme l'exploitant le lit sur la console. Pour les
/// épreuves seulement : aucune route ne le rend.
#[doc(hidden)]
pub fn code_activation_en_cours(state: &AppState) -> Option<String> {
    let etat = state.code_activation_auth.lock().unwrap();
    etat.en_cours
        .as_ref()
        .filter(|c| code_valable(c))
        .map(|c| c.code.clone())
}

/// Pour les épreuves : oublier le délai d'une minute entre deux émissions.
#[doc(hidden)]
pub fn oublier_le_delai_d_emission(state: &AppState) {
    state.code_activation_auth.lock().unwrap().derniere_emission = None;
}

enum VerdictCode {
    Juste,
    Faux,
    /// Trop d'échecs depuis cette adresse.
    Bloque,
}

/// Vérifie un code saisi depuis `adresse`. Juste : consommé. Faux : compté
/// pour le code ET pour l'adresse ; le code est détruit au bout de
/// [`ESSAIS_CODE_ACTIVATION`] essais, l'adresse bloquée au bout de
/// [`ECHECS_PAR_ADRESSE`] échecs sur une heure.
fn verifier_code_activation(
    state: &AppState,
    adresse: std::net::IpAddr,
    saisi: &str,
) -> VerdictCode {
    let mut etat = state.code_activation_auth.lock().unwrap();
    let EtatCodeActivation {
        en_cours,
        echecs_par_ip,
        ..
    } = &mut *etat;
    echecs_par_ip.retain(|_, (_, depuis)| depuis.elapsed() < FENETRE_ECHECS);
    if echecs_par_ip
        .get(&adresse)
        .is_some_and(|(n, _)| *n >= ECHECS_PAR_ADRESSE)
    {
        return VerdictCode::Bloque;
    }
    let juste = match en_cours.as_ref().filter(|c| code_valable(c)) {
        Some(c) => {
            let saisi = normaliser_code(saisi);
            let attendu = c.code.as_bytes();
            let propose = saisi.as_bytes();
            // Comparaison en temps constant ; longueurs comparées en `usize`.
            let mut difference: u8 = u8::from(attendu.len() != propose.len());
            for (i, a) in attendu.iter().enumerate() {
                difference |= a ^ propose.get(i).copied().unwrap_or(0);
            }
            difference == 0
        }
        None => false,
    };
    if juste {
        *en_cours = None;
        return VerdictCode::Juste;
    }
    if let Some(c) = en_cours.as_mut() {
        c.essais_rates += 1;
        if !code_valable(c) {
            *en_cours = None;
        }
    }
    let entree = echecs_par_ip
        .entry(adresse)
        .or_insert((0, std::time::Instant::now()));
    entree.0 += 1;
    VerdictCode::Faux
}

/// Nouveau secret de signature, clé d'API effacée, connexion SSO en attente
/// oubliée : rien de ce qui a été obtenu pendant que tout était ouvert ne
/// survit à l'activation.
fn renouveler_les_secrets(settings: &SettingsRepo) {
    settings
        .set("jwt_secret", &uuid::Uuid::new_v4().to_string())
        .ok();
    settings.delete("api_key").ok();
    for cle in [
        "mozaik_pkce_pending",
        "mozaik_pkce_pending_at",
        "mozaik_redirect_uri_pending",
    ] {
        settings.delete(cle).ok();
    }
    // Celles des profils (`…_profil_{id}`), dates comprises.
    for (cle, _) in settings.all().unwrap_or_default() {
        if cle.starts_with("mozaik_pkce_pending") || cle.starts_with("mozaik_redirect_uri_pending")
        {
            settings.delete(&cle).ok();
        }
    }
}

fn refus_activation_hors_machine() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "activation_code_required",
            "reason": "not_local",
            "message": "Enabling authentication from another device requires the one-time code written to the server console (POST /auth/activation/code; docker logs, journalctl -u tune — on Tune OS over SSH —, or the log file on Windows), or a call from this machine.",
        })),
    )
        .into_response()
}

fn refus_deja_active() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({
            "error": "auth_already_enabled",
            "message": "Authentication is already enabled; the administrator account is managed by signing in.",
        })),
    )
        .into_response()
}

/// Le compte administrateur : le premier profil administrateur (le profil
/// `default`, id 1, sur une installation neuve).
fn compte_administrateur(state: &AppState) -> Option<(i64, String)> {
    state
        .backend
        .query_one(
            "SELECT id, username FROM profiles WHERE is_admin = 1 ORDER BY id LIMIT 1",
            &[],
        )
        .ok()
        .flatten()
        .and_then(|r| {
            Some((
                r.first().and_then(|v| v.as_i64())?,
                r.get(1).and_then(|v| v.as_string())?,
            ))
        })
}

/// Les comptes qui peuvent ouvrir une session — un mot de passe enregistré,
/// ou un compte Mozaiklabs lié (SSO) — et le compte administrateur : ce que
/// l'écran d'activation propose de revoir. Ni l'administrateur ni le
/// propriétaire de la licence ne sont supprimables.
fn comptes_existants(state: &AppState, admin: Option<i64>) -> Vec<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let proprietaire = tune_core::cloud::proprietaire::proprietaire(&settings);
    state
        .backend
        .query_many(
            "SELECT id, username, created_at, is_admin, password_hash, password_hash_v2, email \
             FROM profiles ORDER BY id",
            &[],
        )
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let id = r.first().and_then(|v| v.as_i64())?;
            let nom = r.get(1).and_then(|v| v.as_string())?;
            let cree = r.get(2).and_then(|v| v.as_string());
            let est_admin = r.get(3).and_then(|v| v.as_i64()).unwrap_or(0) != 0
                || r.get(3).and_then(|v| v.as_bool()).unwrap_or(false);
            let a_un_mot_de_passe = [r.get(4), r.get(5)]
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_string())
                .any(|h| !h.is_empty());
            let sso = r
                .get(6)
                .and_then(|v| v.as_string())
                .is_some_and(|e| !e.trim().is_empty());
            let administrateur = est_admin || Some(id) == admin;
            let licence = Some(id) == proprietaire;
            if !a_un_mot_de_passe && !sso && !administrateur {
                // Un profil d'écoute sans mot de passe ni compte lié n'ouvre
                // aucune session.
                return None;
            }
            Some(json!({
                "id": id,
                "username": nom,
                "created_at": cree,
                "role": if administrateur { "admin" } else { "user" },
                "kind": if sso && !a_un_mot_de_passe { "sso" } else { "password" },
                "license_owner": licence,
                "deletable": !administrateur && !licence,
            }))
        })
        .collect()
}

/// `GET /auth/activation` — ce que l'écran d'activation doit afficher.
async fn etat_activation(
    State(state): State<AppState>,
    DepuisCetteMachine(cette_machine): DepuisCetteMachine,
) -> Json<Value> {
    let active = authentification_active(&state);
    Json(json!({
        "auth_enabled": active,
        "from_this_machine": cette_machine,
        "code_required": !active && !cette_machine,
        "admin_username": if active { None } else { compte_administrateur(&state).map(|(_, u)| u) },
        "accounts": if active {
            Vec::new()
        } else {
            comptes_existants(&state, compte_administrateur(&state).map(|(id, _)| id))
        },
        "min_password_length": 8,
    }))
}

/// `POST /auth/activation/code` — écrit un code à usage unique sur la console
/// du serveur. Rien n'est rendu : le code ne se lit que là.
async fn demander_un_code_d_activation(State(state): State<AppState>) -> Response {
    if authentification_active(&state) {
        return refus_deja_active();
    }
    let reponse = |statut: &str| {
        Json(json!({
            "status": statut,
            "where": "server_console",
            "expires_in": DUREE_CODE_ACTIVATION.as_secs(),
            "max_attempts": ESSAIS_CODE_ACTIVATION,
        }))
        .into_response()
    };
    match emettre_code_activation(&state) {
        EmissionCode::Emis => reponse("code_written_to_server_console"),
        EmissionCode::DejaEnCours => reponse("code_already_pending"),
        EmissionCode::TropTot => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "activation_code_too_soon",
                "retry_after": DELAI_ENTRE_EMISSIONS.as_secs(),
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ActivationRequest {
    password: String,
    #[serde(default)]
    code: Option<String>,
    /// Comptes à supprimer avant l'activation (ceux qu'on ne reconnaît pas).
    /// Jamais l'administrateur ni le propriétaire de la licence.
    #[serde(default)]
    delete_accounts: Vec<i64>,
}

/// Une seule activation à la fois : l'état est relu sous ce verrou.
static VERROU_ACTIVATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `POST /auth/activation` — pose le mot de passe du compte administrateur
/// et active l'authentification. Seulement tant qu'elle est coupée, et
/// seulement depuis cette machine ou avec le code de la console.
async fn activer_l_authentification(
    State(state): State<AppState>,
    DepuisCetteMachine(cette_machine): DepuisCetteMachine,
    ConnectInfo(pair): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ActivationRequest>,
) -> Response {
    if authentification_active(&state) {
        return refus_deja_active();
    }
    if body.password.chars().count() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "password_too_short",
                "message": "password must be at least 8 characters",
            })),
        )
            .into_response();
    }
    let Some((id, nom)) = compte_administrateur(&state) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "no_admin_profile"})),
        )
            .into_response();
    };
    // Les comptes à supprimer : seulement des comptes listés par l'écran,
    // jamais l'administrateur ni le propriétaire de la licence. Validé AVANT
    // de consommer le code : une liste fausse ne le brûle pas.
    let supprimables: std::collections::HashSet<i64> = comptes_existants(&state, Some(id))
        .iter()
        .filter(|c| c["deletable"] == true)
        .filter_map(|c| c["id"].as_i64())
        .collect();
    if let Some(intrus) = body
        .delete_accounts
        .iter()
        .find(|i| **i == id || !supprimables.contains(i))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "account_not_deletable",
                "id": intrus,
            })),
        )
            .into_response();
    }
    if !cette_machine {
        let Some(code) = body.code.as_deref().filter(|c| !c.trim().is_empty()) else {
            return refus_activation_hors_machine();
        };
        let adresse = crate::garde_du_relais::adresse_de_l_appelant(pair.ip(), &headers);
        match verifier_code_activation(&state, adresse, code) {
            VerdictCode::Juste => {}
            VerdictCode::Faux => {
                tracing::warn!("activation_authentification_code_refuse");
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "activation_code_invalid",
                        "message": "Invalid or expired activation code.",
                    })),
                )
                    .into_response();
            }
            VerdictCode::Bloque => {
                tracing::warn!("activation_authentification_adresse_bloquee");
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "error": "activation_attempts_exceeded",
                        "message": "Too many wrong activation codes from this address; try again later.",
                    })),
                )
                    .into_response();
            }
        }
    }
    let hachage = match hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("password hashing failed: {e}")})),
            )
                .into_response();
        }
    };

    // Section critique, sans `await` : une seule activation, l'état relu.
    let (token, supprimes) = {
        let _verrou = VERROU_ACTIVATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if authentification_active(&state) {
            return refus_deja_active();
        }
        use tune_core::db::backend::ToSqlValue;
        if let Err(e) = state.backend.execute(
            "UPDATE profiles SET password_hash_v2 = ?, password_hash = NULL WHERE id = ?",
            &[&hachage as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("failed to set password: {e}")})),
            )
                .into_response();
        }
        let mut supprimes = Vec::new();
        let profils = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone());
        for compte in &body.delete_accounts {
            match profils.delete(*compte) {
                Ok(()) => supprimes.push(*compte),
                Err(e) => {
                    tracing::warn!(compte, error = %e, "activation_suppression_compte_echec")
                }
            }
        }
        if !supprimes.is_empty() {
            tracing::warn!(comptes = ?supprimes, "activation_comptes_supprimes");
        }
        let settings = SettingsRepo::with_backend(state.backend.clone());
        renouveler_les_secrets(&settings);
        renouveler_le_jeton_du_pont(&state);
        settings.set("auth_enabled", "true").ok();
        // Un code encore en attente ne sert plus à rien.
        state.code_activation_auth.lock().unwrap().en_cours = None;
        let secret = get_or_create_jwt_secret(&settings);
        (sign_jwt(id, "admin", &secret).ok(), supprimes)
    };
    // Les WebSocket ouvertes pendant que tout était ouvert se ferment : le
    // client se reconnecte avec une session.
    state.fermeture_websockets.send_modify(|n| *n += 1);
    tracing::info!(cette_machine, admin = %nom, "authentification_activee");

    let Some(token) = token else {
        return Json(json!({"auth_enabled": true, "username": nom, "deleted_accounts": supprimes}))
            .into_response();
    };
    let mut response = Json(json!({
        "auth_enabled": true,
        "username": nom,
        "deleted_accounts": supprimes,
        "token": token,
        "token_type": "Bearer",
        "expires_in": 86400,
        "role": "admin",
    }))
    .into_response();
    let cookie = session_cookie(&token, &headers);
    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie.parse().unwrap());
    response
}

#[cfg(test)]
mod cette_machine_tests {
    use super::adresse_de_cette_machine;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn seul_le_bouclage_est_cette_machine() {
        for ok in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            assert!(adresse_de_cette_machine(ip(ok)), "{ok}");
        }
        for ko in [
            "192.168.1.50",
            "10.0.0.1",
            "169.254.169.254",
            "::ffff:192.168.1.50",
            "fe80::1",
            "fd00::1",
            "203.0.113.5",
            "0.0.0.0",
            "::",
        ] {
            assert!(!adresse_de_cette_machine(ip(ko)), "{ko}");
        }
    }
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
        assert!(is_public_auth_route(&Method::GET, "/api/v1/auth/config"));
        // Nesting may strip the /api/v1 prefix — both spellings must hold.
        assert!(is_public_auth_route(&Method::POST, "/auth/login"));
    }

    #[test]
    fn privileged_auth_routes_are_never_public() {
        // The P0: these must never be reachable without a token.
        assert!(!is_public_auth_route(&Method::POST, "/api/v1/auth/config"));
        // Sécurité : l'inscription n'est plus publique sous authentification.
        assert!(!is_public_auth_route(
            &Method::POST,
            "/api/v1/auth/register"
        ));
        assert!(!is_public_auth_route(&Method::POST, "/auth/register"));
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
