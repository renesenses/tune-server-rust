use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::cloud::plugins::PluginMarketplace;
use tune_core::cloud::sso::{MozaikAuth, PkceSession};
use tune_core::cloud::telemetry::TelemetryReporter;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sso/authorize", get(sso_authorize))
        .route("/sso/callback", get(sso_callback))
        .route("/sso/status", get(sso_status))
        .route("/sso/disconnect", post(sso_disconnect))
        .route("/telemetry/status", get(telemetry_status))
        .route("/telemetry/enable", post(telemetry_enable))
        .route("/telemetry/disable", post(telemetry_disable))
        .route("/plugins", get(marketplace_list))
        .route("/plugins/{name}/install", post(marketplace_install))
        .route("/plugins/{name}/vote", post(marketplace_vote))
        .route("/community/artist-image", post(report_artist_image))
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
        .route("/library-sync/reconcile", post(library_sync_reconcile))
}

// ---------------------------------------------------------------------------
// SSO
// ---------------------------------------------------------------------------

/// Resolve the Mozaik OAuth client id.
///
/// Precedence: `mozaik_client_id` setting → `TUNE_MOZAIK_CLIENT_ID` env →
/// baked-in [`sso::DEFAULT_CLIENT_ID`]. Empty at every level ⇒ SSO stays
/// unconfigured and degrades gracefully (opt-in, never blocking).
fn resolve_client_id(settings: &SettingsRepo) -> Option<String> {
    settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_MOZAIK_CLIENT_ID").ok())
        .or_else(|| Some(tune_core::cloud::sso::DEFAULT_CLIENT_ID.to_string()))
        .filter(|s| !s.is_empty())
}

fn get_mozaik_auth(settings: &SettingsRepo) -> Option<MozaikAuth> {
    let client_id = resolve_client_id(settings)?;
    let base_url = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());
    Some(MozaikAuth::new(client_id, Some(&base_url)))
}

/// OAuth redirect URI for the SSO round-trip.
///
/// Precedence: explicit `mozaik_redirect_uri` setting → the address the browser
/// actually used to reach Tune (the request `Host` header) → the RFC 8252
/// loopback default `127.0.0.1:{port}`.
///
/// The loopback default (RFC 8252 §7.3) only works when the browser and server
/// share a machine. When Tune runs on a remote box reached over the LAN
/// (`192.168.x.x:8888`), a `127.0.0.1` redirect lands on the *browser's* own
/// loopback and the login "spins" (Fabien). Deriving the host from the request
/// keeps that case working — provided the OAuth server (Passport) accepts the
/// resulting non-loopback redirect URI.
fn redirect_uri(state: &AppState, headers: Option<&HeaderMap>) -> String {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(explicit) = settings
        .get("mozaik_redirect_uri")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
    {
        return explicit;
    }
    let host = headers
        .and_then(|h| h.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty())
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("127.0.0.1:{}", state.port));
    format!("http://{host}/api/v1/cloud/sso/callback")
}

async fn sso_authorize(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let Some(auth) = get_mozaik_auth(&settings) else {
        // sso_authorize is a full browser navigation, so degrade to a friendly
        // HTML page instead of raw JSON when Cloud SSO isn't provisioned on this
        // server (mozaik_client_id unset). Covers every entry point at once.
        let html = r#"<!doctype html><html lang="fr"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Cloud bientôt disponible</title>
<style>body{font-family:system-ui,-apple-system,sans-serif;background:#111;color:#eee;display:flex;min-height:100vh;margin:0;align-items:center;justify-content:center;text-align:center}.card{max-width:440px;padding:2rem}h1{font-weight:600}a{color:#6ab0ff;text-decoration:none}a:hover{text-decoration:underline}</style>
</head><body><div class="card">
<h1>Cloud bientôt disponible</h1>
<p>La connexion mozaiklabs.fr n'est pas encore activée sur ce serveur.</p>
<p><a href="https://mozaiklabs.fr">En savoir plus</a> &nbsp;·&nbsp; <a href="/">Retour à Tune</a></p>
</div></body></html>"#;
        return (StatusCode::SERVICE_UNAVAILABLE, Html(html)).into_response();
    };

    // PKCE (RFC 7636): mint a fresh verifier/challenge/state and stash the
    // verifier + state for the browser round-trip (consumed in sso_callback).
    let pkce = PkceSession::generate();
    settings
        .set(
            "mozaik_pkce_pending",
            &serde_json::to_string(&pkce).unwrap_or_default(),
        )
        .ok();

    let uri = redirect_uri(&state, Some(&headers));
    // Persist the exact redirect_uri: OAuth requires the token exchange in the
    // callback to present an identical value.
    settings.set("mozaik_redirect_uri_pending", &uri).ok();
    let url = auth.authorize_url(&uri, &pkce.challenge, &pkce.state);
    Redirect::temporary(&url).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
    state: Option<String>,
}

async fn sso_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
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

    // Load & validate the pending PKCE session (CSRF: state must match), then
    // consume it — one-shot, cleared regardless of the exchange outcome.
    let pending: Option<PkceSession> = settings
        .get("mozaik_pkce_pending")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    settings.set("mozaik_pkce_pending", "").ok();

    let Some(pkce) = pending else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no pending SSO session (restart login)"})),
        )
            .into_response();
    };
    if q.state.as_deref() != Some(pkce.state.as_str()) {
        warn!("sso_state_mismatch");
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "state mismatch (possible CSRF)"})),
        )
            .into_response();
    }

    // Reuse the exact redirect_uri minted at authorize time (must match); fall
    // back to deriving it from this request's Host if the pending value is gone.
    let uri = settings
        .get("mozaik_redirect_uri_pending")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| redirect_uri(&state, Some(&headers)));
    settings.set("mozaik_redirect_uri_pending", "").ok();

    let token = match auth.exchange_code(&code, &uri, &pkce.verifier).await {
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

    // 🔴 AVANT d'écraser `mozaik_user`, et pas après.
    //
    // L'adoption reprend le compte DÉJÀ lié — celui d'une installation qui en
    // portait un avant que `owner_profile_id` n'existe. La placer plus bas la
    // ferait lire le compte qui vient d'arriver, c'est-à-dire adopter le
    // nouveau venu : exactement ce qu'elle est là pour empêcher.
    tune_core::cloud::proprietaire::adopter_le_compte_lie(&settings, &state.backend);

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
        // Update display name from the cloud profile — but NOT avatar_path.
        // The `avatar_path` column stores the profile's chosen avatar COLOR
        // (hex), and the client fetches the cloud avatar image separately from
        // /cloud/sso/status. Overwriting avatar_path with the SSO avatar URL
        // destroyed the user's chosen colour on every SSO login (Bilou:
        // "couleur de fond du profil non mémorisée après connexion").
        state
            .backend
            .execute(
                "UPDATE profiles SET display_name = ? WHERE id = ?",
                &[
                    &user.display_name as &dyn ToSqlValue,
                    &id as &dyn ToSqlValue,
                ],
            )
            .ok();
        id
    } else {
        // Create new local profile from cloud user. Seed avatar_path with a
        // default colour (not the SSO URL) so the fallback avatar circle has a
        // valid colour when disconnected; the cloud image is shown separately.
        let default_color = "#6366f1";
        // `is_admin` est lie en ENTIER, pas en booleen. La colonne vaut
        // SMALLINT sur toute installation PostgreSQL native (005), et
        // PostgreSQL REFUSE l'affectation `boolean -> smallint` :
        //
        //   ERROR:  column "is_admin" is of type smallint but expression
        //           is of type boolean
        //
        // Mesure du 11/09/2026 sur PostgreSQL 16.15 : cet INSERT echouait
        // sur toute base PostgreSQL NATIVE, donc aucune premiere connexion
        // SSO n'y creait de profil (#3726). Un entier passe sur les trois
        // formes que porte cette colonne dans le parc — SMALLINT (natif),
        // TEXT (base migree, `bigint -> text` est accepte) et INTEGER
        // (SQLite) — et les lecteurs passent tous par `as_i64()`.
        let is_admin: i64 = i64::from(user.is_admin);
        state
            .backend
            .execute_returning_id(
                "INSERT INTO profiles (username, display_name, email, avatar_path, is_admin) VALUES (?, ?, ?, ?, ?)",
                &[&user.email as &dyn ToSqlValue, &user.display_name as &dyn ToSqlValue, &user.email as &dyn ToSqlValue, &default_color as &dyn ToSqlValue, &is_admin as &dyn ToSqlValue],
            )
            .unwrap_or(0)
    };

    // ── L'état de LICENCE n'appartient qu'au propriétaire ────────────────
    //
    // 🔴 Ces trois écritures sont des états de SERVEUR, pas de compte. Tant
    // qu'elles s'exécutaient à chaque connexion, un second compte gratuit
    // remettait `mozaik_premium` à faux et effaçait l'échéance — puis le
    // battement de fond, qui relit le jeton global (désormais celui du second
    // compte), confirmait le palier gratuit toutes les heures. Le premium par
    // compte ne redescendait pas le temps d'un rafraîchissement : il restait
    // par terre.
    //
    // Elles sont donc déplacées ici, APRÈS la résolution du profil : on
    // établit d'abord qui se connecte, on décide ensuite de ce qu'il a le
    // droit de changer. Le premier compte lié devient propriétaire — d'où
    // l'invariance sur une installation à un seul compte.
    let pilote = tune_core::cloud::proprietaire::revendiquer(&settings, profile_id);

    if pilote {
        // Unlock premium from the linked account (SSO) when the server reports it.
        // OR-ed with the license-key path — never downgrades a keyed premium.
        state
            .license
            .set_account_premium(user.premium, user.license_expires_at.clone())
            .await;

        // Droits de MODULE payants (SKU distincts du palier, ex. la sortie
        // Diretta). Ils voyagent avec le compte, jamais avec la clé : sans cet
        // appel, `set_modules` n'avait qu'un seul appelant — le battement de fond
        // (`background.rs`, HEARTBEAT_INTERVAL = 1 h). Un compte qui vient d'être
        // lié s'entendait donc répondre « module_not_owned / purchase_module »
        // pendant une heure pour un module qu'il possède (#2138).
        state.license.set_modules(user.modules.clone()).await;

        // Qobuz endpoint order (founder flag): persist and push into the live
        // QobuzService so the order applies without a restart.
        state
            .license
            .set_qobuz_proxy_first(user.qobuz_proxy_first)
            .await;
        crate::background::apply_qobuz_proxy_first(&state.services, user.qobuz_proxy_first).await;
    } else {
        // La liaison RÉUSSIT quand même — identité, photo, préférences. Seule
        // la licence ne bouge pas. Un refus complet serait pire : il rendrait
        // le multi-compte impossible pour rien.
        tracing::info!(
            profile_id,
            email = %user.email,
            "sso_licence_non_pilotee_par_ce_profil"
        );
    }

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
    let configured = resolve_client_id(&settings).is_some();
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

/// POST /cloud/sso/disconnect — log out of the mozaiklabs.fr account: drop the
/// stored tokens/profile and revoke the account premium. The license-key path is
/// untouched (a keyed premium survives).
async fn sso_disconnect(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for key in [
        "mozaik_access_token",
        "mozaik_refresh_token",
        "mozaik_user",
        "mozaik_pkce_pending",
    ] {
        settings.delete(key).ok();
    }
    state.license.clear_account_premium().await;
    // Founder endpoint order came from this account — revert to direct-first.
    state.license.set_qobuz_proxy_first(false).await;
    crate::background::apply_qobuz_proxy_first(&state.services, false).await;
    info!("sso_disconnected");
    Json(json!({ "connected": false }))
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

/// #3383 — `enabled` dit desormais l'etat EFFECTIF, celui que les gardes
/// d'envoi consultent, et non plus la seule variable d'environnement.
///
/// `env_override` est ajoute pour repondre a la question que l'issue laissait
/// ouverte : que montrer quand l'exploitant impose un etat que l'utilisateur
/// ne peut pas changer. Vrai = `TUNE_TELEMETRY` coupe a l'echelle de la
/// machine, la bascule de l'interface ne peut rien rallumer. Champ AJOUTE :
/// un client qui l'ignore lit `enabled` comme avant.
async fn telemetry_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = TelemetryReporter::is_enabled_for(&settings);
    let server_id = settings.get("server_id").ok().flatten();
    let rate_limits = tune_core::cloud::rate_limit::active_all(&settings);
    Json(json!({
        "enabled": enabled,
        "env_override": !TelemetryReporter::is_enabled(),
        "server_id": server_id,
        "rate_limits": rate_limits,
    }))
}

/// Ecrit le consentement, au lieu de se contenter de le relire (#3383).
///
/// `TUNE_TELEMETRY=false` reste souverain : la reponse renvoie l'etat
/// EFFECTIF, donc `false`, et l'interface voit tout de suite que son clic n'a
/// pas pris — au lieu de basculer une case que le rafraichissement suivant
/// remettra en place toute seule.
async fn telemetry_enable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "true")
        .ok();
    TelemetryReporter::get_or_create_server_id(&settings);
    info!("telemetry_enabled");
    Json(json!({
        "enabled": TelemetryReporter::is_enabled_for(&settings),
        "env_override": !TelemetryReporter::is_enabled(),
    }))
}

/// Le refus est ECRIT, et les sept gardes d'envoi le lisent (#3383).
///
/// Avant ce correctif, cette route ne liait meme pas son `State` : elle
/// journalisait « posez TUNE_TELEMETRY=false » et repondait `enabled: true`
/// juste apres que l'utilisateur eut demande l'inverse.
///
/// Ce qui continue de partir, et c'est deliberé : la revalidation horaire de
/// la cle de licence (trois champs, rien de descriptif) et le rafraichissement
/// des droits premium SSO. Un opt-out ne doit jamais se payer en
/// fonctionnalites perdues — voir `background::heartbeat_plan` (LIC-1).
async fn telemetry_disable(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .ok();
    info!("telemetry_disabled");
    Json(json!({
        "enabled": TelemetryReporter::is_enabled_for(&settings),
        "env_override": !TelemetryReporter::is_enabled(),
    }))
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
    headers: HeaderMap,
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
            crate::routes::cloud_error::reponse(&e, &headers, StatusCode::BAD_GATEWAY, json!({}))
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
    headers: HeaderMap,
    Json(body): Json<VoteRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let mp = PluginMarketplace::new(base_url.as_deref());

    match mp.vote(&name, body.up).await {
        Ok(()) => Json(json!({"name": name, "voted": true, "up": body.up})).into_response(),
        Err(e) => {
            crate::routes::cloud_error::reponse(&e, &headers, StatusCode::BAD_GATEWAY, json!({}))
        }
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
    headers: HeaderMap,
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
        Err(e) => {
            crate::routes::cloud_error::reponse(&e, &headers, StatusCode::BAD_GATEWAY, json!({}))
        }
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
    headers: HeaderMap,
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
            crate::routes::cloud_error::reponse(&e, &headers, StatusCode::BAD_GATEWAY, json!({}))
        }
    }
}

#[derive(Deserialize)]
struct CoverSyncRequest {
    since: Option<String>,
}

async fn sync_community_covers(
    State(state): State<AppState>,
    headers: HeaderMap,
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
                return crate::routes::cloud_error::reponse(
                    &e,
                    &headers,
                    StatusCode::BAD_GATEWAY,
                    json!({}),
                );
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
                .get()
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
                "available": f.available(),
            }),
        );
    }

    // Meme lecture que `/system/config` et que le refus de lecture : une seule
    // implementation du plafond (#3673). Cette route-ci en tenait sa propre
    // copie ternaire, identique a celle de `/system/config` a un `serde_json::
    // Value::Null` pres — deux ecritures d'une meme regle, donc deux occasions
    // de deriver.
    let zone_limit = state.license.limite_zones().await;

    // Floating-license single-session model: when set, `tier` above is already
    // Free (premium is gated off here) and this object tells the UI WHY — the
    // license is currently active on another of the user's servers.
    let session_conflict = ls.session_conflict.as_ref().map(|c| {
        json!({
            "active_server": c.active_server,
            "active_since": c.active_since,
        })
    });

    // Grâce hors ligne (#1999) : décrit la fenêtre de revalidation déjà en
    // vigueur, pour que l'utilisateur sache qu'il est couvert, depuis quand et
    // jusqu'à quand — au lieu de découvrir la dégradation le jour où une
    // fonction cesse de répondre. Purement descriptif : ne change ni la durée,
    // ni l'instant d'expiration, ni ce qui est désactivé. `null` quand la
    // question ne se pose pas (Free, ou abonnement réellement échu).
    let offline_grace = tune_core::license::offline_grace(&ls);

    Json(json!({
        "tier": ls.tier,
        "license_key": ls.license_key,
        "expires_at": ls.expires_at,
        "last_validated": ls.last_validated,
        "hardware_fingerprint": ls.hardware_fingerprint,
        "features": features,
        "zone_limit": zone_limit,
        "session_conflict": session_conflict,
        "offline_grace": offline_grace,
    }))
}

/// POST /cloud/license/activate — enregistre la clé **et** la fait confirmer en
/// ligne dans le même aller-retour.
///
/// C'est la route qu'appelle le panneau « Tune Premium License » du client web
/// (`activateLicense()`), et la seule que ce panneau appelle au moment où
/// l'utilisateur colle sa clé.
///
/// Elle se contentait de `set_license_key`, qui depuis c15dcc61 (« require
/// online validation before Premium ») stocke la clé **en attente** — palier
/// Free, aucune validation. La route rendait donc `tier: "free"` pour une clé
/// parfaitement valide, et le panneau annonçait « invalide » au premier essai
/// (#1279, Alex Campbell, licence Lifetime). Il fallait ensuite actionner
/// « Valider » (`/cloud/license/validate`) pour obtenir l'aller-retour manquant.
/// La route jumelle `POST /system/license` avait, elle, reçu l'appel à
/// `validate_stored_license` dans ce même commit ; celle-ci était restée en
/// arrière. Les deux font désormais la même chose.
///
/// Ce n'est pas un simple confort d'affichage : le battement de cœur, seul
/// autre chemin qui promeut une clé en attente, ne part pas quand la télémétrie
/// est refusée (`heartbeat_plan`, `send_heartbeat: false`). Sans validation
/// ici, une clé posée sur une machine sans télémétrie restait en attente
/// indéfiniment.
///
/// Le palier rendu est le palier **effectif** relu après validation, jamais une
/// promesse locale : serveur de licences injoignable ou clé refusée ⇒ `pending`
/// et Free, exactement comme avant. Aucune clé ne débloque quoi que ce soit
/// sans confirmation en ligne.
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
    let tier = validate_stored_license(&state).await;
    let premium = tier == tune_core::license::Tier::Premium;
    let ls = state.license.license_state().await;
    // `validate_stored_license` a déjà émis l'événement quand le serveur de
    // licences a tranché. On ne l'émet ici que dans le cas contraire : la clé a
    // tout de même changé, les écoutants doivent relire le statut.
    if !premium {
        state.event_bus.emit(
            "license.updated",
            json!({"tier": ls.tier, "expires_at": ls.expires_at}),
        );
    }
    Json(json!({
        "status": if premium { "activated" } else { "pending" },
        "tier": ls.tier,
        "expires_at": ls.expires_at,
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

/// URL du point de validation de licence chez mozaiklabs.fr.
///
/// Le réglage `mozaik_base_url` la redirige — le même réglage que le SSO, le
/// marché de greffons et les couvertures communautaires honorent déjà partout
/// dans ce fichier. L'URL était la seule de cloud.rs à rester en dur, ce qui
/// rendait l'activation de licence intestable autrement qu'en appelant le
/// serveur de licences de production.
pub(crate) fn license_validate_url(settings: &SettingsRepo) -> String {
    let base = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string());
    format!("{}/api/v1/license/validate", base.trim_end_matches('/'))
}

/// Validate the currently-stored license key against mozaiklabs.fr and apply
/// the authoritative tier via `update_from_server`. Returns the effective tier
/// afterwards. On an unreachable/erroring server it leaves the cached tier
/// untouched (a freshly-entered, still-pending key therefore stays Free until a
/// genuine online confirmation). Shared by the on-demand validate route and the
/// "set key" route so a newly-entered key is confirmed immediately instead of
/// unlocking Premium locally with no server round-trip.
pub(crate) async fn validate_stored_license(state: &AppState) -> tune_core::license::Tier {
    let ls = state.license.license_state().await;
    let Some(key) = ls.license_key.clone() else {
        return tune_core::license::Tier::Free;
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
    let payload = json!({
        "license_key": key,
        "hardware_fingerprint": ls.hardware_fingerprint,
        "server_id": server_id,
        "version": tune_core::version(),
    });

    let resp = state
        .http_client
        .post(license_validate_url(&settings))
        .timeout(std::time::Duration::from_secs(10))
        .json(&payload)
        .send()
        .await;
    let Ok(resp) = resp else {
        return state.license.tier().await;
    };
    if !resp.status().is_success() {
        return state.license.tier().await;
    }
    let Ok(body) = resp.json::<Value>().await else {
        return state.license.tier().await;
    };

    // Lecture unique du verdict (`tune_core::license::verdict_licence`) : ce
    // corps etait interprete a quatre endroits, avec des defauts opposes sur le
    // champ manquant. Ici la politique est inchangee : rien n'est persiste hors
    // d'une confirmation, et un refus — transitoire ou meme expire — laisse le
    // palier en cache tel quel. La cle vient d'etre posee en attente par
    // `set_license_key`, donc le palier effectif est deja Free.
    let tune_core::license::VerdictLicence::Confirmee { tier, expires_at } =
        tune_core::license::verdict_licence(&body)
    else {
        return state.license.tier().await;
    };
    state
        .license
        .update_from_server(tier, expires_at.clone())
        .await;
    state.event_bus.emit(
        "license.updated",
        json!({"tier": tier, "expires_at": expires_at}),
    );
    tier
}

// ---------------------------------------------------------------------------
// Le corps de `POST /cloud/license/validate` — le seul porteur de vérité
// ---------------------------------------------------------------------------

/// Pourquoi `POST /cloud/license/validate` a rendu ce qu'il a rendu (#3906).
///
/// Cette route répond **HTTP 200 dans toutes ses sorties d'échec**, et c'est
/// délibéré : lever le statut ferait jeter la réponse par les deux clients
/// déployés — `TuneAPIClient.checkResponse` côté Swift et le client web —
/// qui abandonnent sur un code d'erreur alors qu'ils lisent aujourd'hui le
/// corps. L'arbitrage est tenu : **le code de statut ne bouge pas**. Ce qui
/// manquait est ailleurs — le corps ne portait pas de quoi distinguer les
/// sorties les unes des autres.
///
/// `status` ne prend que **cinq** valeurs (`validated`, `invalid`, `cached`,
/// `error`, `no_license`) pour **neuf** sorties. `cached` en recouvre trois
/// (point d'accès en 404, verdict absent, refus non autoritaire) et `error`
/// trois autres (requête qui n'aboutit pas, statut distant, corps illisible).
/// Un client qui veut les séparer n'avait qu'une issue : analyser le champ
/// `message`, composé **en anglais** — ce que le client web déployé fait
/// aujourd'hui pour retrouver un 429 (`statutDistantDepuisMessage`,
/// tune-web-client v0.9.146).
///
/// Trois champs lèvent l'ambiguïté, sans toucher au statut HTTP :
///
/// - `reason` — un code stable, **un par sortie**, jamais traduit ;
/// - `ok` — vrai seulement quand le verdict distant a été **posé** ici ;
/// - `upstream_status` — le statut de mozaiklabs.fr **en nombre**, pour que
///   personne n'ait plus à lire une phrase anglaise pour distinguer un
///   plafond de requêtes (429) d'une panne (502).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotifValidation {
    /// Aucune clé n'est enregistrée sur ce serveur.
    AucuneCle,
    /// La requête vers mozaiklabs.fr n'a pas abouti : réseau, DNS, délai.
    RequeteEchouee,
    /// Le point d'accès distant répond 404 : rien n'est conclu.
    PointAccesAbsent,
    /// mozaiklabs.fr a répondu un statut d'erreur — 429 compris.
    StatutDistant,
    /// La réponse distante n'est pas un JSON lisible.
    ReponseIllisible,
    /// Le corps ne porte pas `license_valid` : le serveur ne s'est pas prononcé.
    SansVerdict,
    /// `license_valid:false` **nu** : ni révocation, ni confirmation.
    RefusTransitoire,
    /// Expiration passée confirmée : le palier est ramené à `free`.
    Expiree,
    /// Le verdict distant est posé.
    Confirmee,
}

impl MotifValidation {
    /// Le discriminant **stable** que le client teste. `snake_case`, jamais
    /// traduit, jamais recomposé à partir d'une phrase.
    fn code(self) -> &'static str {
        match self {
            Self::AucuneCle => "no_license_key",
            Self::RequeteEchouee => "request_failed",
            Self::PointAccesAbsent => "endpoint_not_found",
            Self::StatutDistant => "upstream_error",
            Self::ReponseIllisible => "unreadable_response",
            Self::SansVerdict => "no_verdict",
            Self::RefusTransitoire => "rejected_transient",
            Self::Expiree => "expired",
            Self::Confirmee => "confirmed",
        }
    }

    /// Le `status` historique, **inchangé** : deux clients déployés le lisent,
    /// et le client web en fait déjà cinq écrans distincts. `reason` s'ajoute
    /// à côté ; il ne le remplace pas.
    fn status(self) -> &'static str {
        match self {
            Self::AucuneCle => "no_license",
            Self::RequeteEchouee | Self::StatutDistant | Self::ReponseIllisible => "error",
            Self::PointAccesAbsent | Self::SansVerdict | Self::RefusTransitoire => "cached",
            Self::Expiree => "invalid",
            Self::Confirmee => "validated",
        }
    }

    /// Vrai quand l'aller-retour a **abouti** et que le verdict distant a été
    /// appliqué ici.
    ///
    /// Ce n'est pas la promesse d'un palier premium : un compte confirmé en
    /// `free` rend `ok:true` avec `tier:"free"`. C'est la réponse à la seule
    /// question que le bouton « Valider » posait sans pouvoir l'obtenir —
    /// « la validation a-t-elle eu lieu ? ». `Expiree` rend `false` : le
    /// serveur a bien parlé, mais la clé ne vaut rien.
    fn succes(self) -> bool {
        matches!(self, Self::Confirmee)
    }
}

/// Le corps rendu par la route, augmenté des champs que le client teste.
///
/// Fonction **pure** : c'est elle que les essais exercent, sans axum, sans
/// réseau et sans base. `status`, `reason` et `ok` sont posés **après** le
/// corps d'origine, donc un site d'appel distrait ne peut pas les contredire.
/// `upstream_status` est toujours présent — `null` quand aucun aller-retour
/// n'a eu lieu — pour qu'un client typé n'ait pas à distinguer « absent » de
/// « nul ».
fn corps_validation(motif: MotifValidation, corps: Value) -> Value {
    let mut objet = match corps {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    objet
        .entry("upstream_status".to_string())
        .or_insert(Value::Null);
    objet.insert("status".to_string(), json!(motif.status()));
    objet.insert("reason".to_string(), json!(motif.code()));
    objet.insert("ok".to_string(), json!(motif.succes()));
    Value::Object(objet)
}

/// La réponse HTTP. **200 dans les neuf cas** — voir [`MotifValidation`].
fn reponse_validation(motif: MotifValidation, corps: Value) -> axum::response::Response {
    Json(corps_validation(motif, corps)).into_response()
}

/// Le message du cas « statut distant », mot pour mot.
///
/// ⚠️ Le client web déployé (tune-web-client v0.9.146,
/// `statutDistantDepuisMessage`) extrait le code distant de cette phrase par
/// `^Server returned (\d{3})\b` : c'est aujourd'hui son seul moyen de
/// distinguer un plafond de requêtes d'une panne. Le corps porte désormais
/// `upstream_status`, mais tant que ce client tourne, reformuler cette phrase
/// lui retire la distinction **sans un seul rouge**. D'où la garde
/// `le_message_du_statut_distant_garde_le_prefixe_que_le_client_analyse`.
fn message_statut_distant(status: StatusCode) -> String {
    format!("Server returned {status}")
}

/// POST /cloud/license/validate
///
/// Déclenche une validation immédiate contre mozaiklabs.fr et rend le palier
/// autoritaire, ou l'état en cache si le serveur est injoignable.
///
/// 🔴 **Répond 200 dans ses neuf sorties** : c'est le contrat, pas un oubli
/// (#3906). La vérité vit dans le corps, et le champ qui la porte est
/// `reason` — voir [`MotifValidation`].
async fn license_validate(State(state): State<AppState>) -> impl IntoResponse {
    let ls = state.license.license_state().await;
    let Some(ref key) = ls.license_key else {
        return reponse_validation(
            MotifValidation::AucuneCle,
            json!({
                "tier": "free",
                "message": "No license key configured",
            }),
        );
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
        .post(license_validate_url(&settings))
        .timeout(std::time::Duration::from_secs(10))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "license_validate_request_failed");
            return reponse_validation(
                MotifValidation::RequeteEchouee,
                json!({
                    "tier": ls.tier,
                    "message": format!("Validation request failed: {e}"),
                    "cached": true,
                }),
            );
        }
    };

    // 404 means the server endpoint doesn't exist yet — keep cached state.
    if resp.status() == StatusCode::NOT_FOUND {
        info!("license_validate_endpoint_not_found, keeping cached state");
        return reponse_validation(
            MotifValidation::PointAccesAbsent,
            json!({
                "tier": ls.tier,
                "message": "Validation endpoint not available yet",
                "cached": true,
                "upstream_status": StatusCode::NOT_FOUND.as_u16(),
            }),
        );
    }

    if !resp.status().is_success() {
        let status = resp.status();
        warn!(status = %status, "license_validate_server_error");
        return reponse_validation(
            MotifValidation::StatutDistant,
            json!({
                "tier": ls.tier,
                "message": message_statut_distant(status),
                "cached": true,
                "upstream_status": status.as_u16(),
            }),
        );
    }

    // Parse the server's authoritative response.
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "license_validate_parse_failed");
            return reponse_validation(
                MotifValidation::ReponseIllisible,
                json!({
                    "tier": ls.tier,
                    "message": format!("Failed to parse response: {e}"),
                    "cached": true,
                }),
            );
        }
    };

    // Meme lecture du verdict que les trois autres appelants
    // (`tune_core::license::verdict_licence`). Celle-ci etait la seule a
    // defaillir en ACCORDANT : `license_valid` absent valait `true`, et comme
    // `license_tier` absent valait `"free"`, un corps sans champ de licence
    // — point d'acces redirige, enveloppe d'erreur rendue en 200, schema qui a
    // bouge — persistait Free sur un compte premium **et** repondait
    // `status:"validated"`. Le bouton « Valider » du panneau retrogradait donc
    // un payeur en annoncant un succes. Un verdict absent ne persiste plus
    // rien : le palier en cache est rendu tel quel, sous `status:"cached"`.
    let verdict = tune_core::license::verdict_licence(&body);
    let (tier, expires_at) = match verdict {
        tune_core::license::VerdictLicence::Absent => {
            warn!("license_validate_sans_verdict_palier_conserve");
            return reponse_validation(
                MotifValidation::SansVerdict,
                json!({
                    "tier": ls.tier,
                    "message": "The licence server returned no verdict; the cached tier is kept.",
                    "cached": true,
                }),
            );
        }
        tune_core::license::VerdictLicence::Expiree => {
            info!("license_invalidated_by_server_validate (authoritative expiry)");
            state
                .license
                .update_from_server(tune_core::license::Tier::Free, None)
                .await;
            state.event_bus.emit(
                "license.updated",
                json!({"tier": "free", "expires_at": null}),
            );
            return reponse_validation(
                MotifValidation::Expiree,
                json!({
                    "tier": "free",
                    "message": "License key is not valid",
                }),
            );
        }
        tune_core::license::VerdictLicence::RefusTransitoire => {
            // Un refus nu n'est pas une revocation : Premium survit dans la
            // fenetre de grace au lieu d'etre detruit sur un hoquet.
            warn!("license_validate_rejected_keeping_cached_tier");
            return reponse_validation(
                MotifValidation::RefusTransitoire,
                json!({
                    "tier": ls.tier,
                    "message": "Server could not confirm the license right now; Premium is retained (grace period).",
                    "cached": true,
                }),
            );
        }
        tune_core::license::VerdictLicence::Confirmee { tier, expires_at } => (tier, expires_at),
    };

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
    reponse_validation(
        MotifValidation::Confirmee,
        json!({
            "tier": updated.tier,
            "expires_at": updated.expires_at,
            "last_validated": updated.last_validated,
        }),
    )
}

/// #3906 — les neuf sorties de `POST /cloud/license/validate` doivent être
/// **distinguables par le corps**, puisque le code de statut vaut 200 pour
/// toutes.
///
/// Ces essais ne tiennent ni base ni réseau : [`corps_validation`] est pure,
/// et la garde de site est textuelle. Ils tournent donc dans le job `Test`
/// (`ci.yml` l. 262, `-p tune-server`), pas seulement dans `ci:full`.
#[cfg(test)]
mod tests_licence_proprietaire {
    /// 🔴 Le garde de CÂBLAGE, et c'est le seul qui voit le vrai défaut.
    ///
    /// `tune_core::cloud::proprietaire` a ses propres tests, mais ils restent
    /// verts si personne n'appelle le module : une règle juste que le chemin
    /// réel n'emprunte pas ne protège de rien. Même leçon que
    /// `state.rs::le_demarrage_publie_bien_le_client_de_relais`, où un `.set()`
    /// retiré laissait la batterie au vert.
    ///
    /// La ronde SSO ne peut pas être jouée ici — elle exige mozaiklabs.fr, un
    /// échange OAuth et une base — donc la garde porte sur la source.
    #[test]
    fn la_licence_n_est_ecrite_que_par_le_proprietaire() {
        let source = include_str!("cloud.rs");

        assert!(
            source.contains("proprietaire::revendiquer(&settings, profile_id)"),
            "sso_callback doit demander QUI pilote la licence, et le demander \
             avec le profil resolu par le COURRIEL (profile_id) — pas avec le \
             profil choisi a l'ecran"
        );

        // Les trois ecritures d'etat serveur doivent etre SOUS la condition.
        let garde = source
            .find("if pilote {")
            .expect("le garde `if pilote` a disparu : toute connexion reecrit la licence");
        let sinon = source[garde..]
            .find("} else {")
            .expect("le garde n'a plus de branche `else`");
        let gardees = &source[garde..garde + sinon];
        for ecriture in [
            "set_account_premium",
            "set_modules",
            "set_qobuz_proxy_first",
        ] {
            assert!(
                gardees.contains(ecriture),
                "`{ecriture}` est sortie du garde : un second compte ferait \
                 retomber le palier de tout le serveur"
            );
        }
    }

    /// 🔴 L'adoption doit lire l'ANCIEN compte lié.
    ///
    /// Defaut que je me suis fait a moi-meme le 12/09/2026 en placant l'appel
    /// apres la mise a jour de `mozaik_user` : l'adoption y lisait le compte
    /// qui venait d'arriver et adoptait le nouveau venu — soit precisement
    /// l'inverse de sa raison d'etre, qui est de garder la licence au
    /// proprietaire historique d'une installation deja liee.
    #[test]
    fn l_adoption_precede_l_ecrasement_du_compte() {
        let source = include_str!("cloud.rs");
        let adoption = source
            .find("proprietaire::adopter_le_compte_lie(&settings, &state.backend)")
            .expect("la reprise des installations existantes a disparu");
        let ecrasement = source
            .find(".set(\"mozaik_access_token\"")
            .expect("l'ecriture du jeton a disparu");
        assert!(
            adoption < ecrasement,
            "l'adoption lit `mozaik_user` APRES sa reecriture : elle adopterait \
             le compte qui vient de se connecter au lieu de celui deja en place"
        );
    }

    /// L'ordre compte : on etablit QUI se connecte avant de decider ce qu'il a
    /// le droit de changer. Ecrire la licence avant de resoudre le profil
    /// rendrait le garde impossible a evaluer.
    #[test]
    fn le_profil_est_resolu_avant_la_licence() {
        let source = include_str!("cloud.rs");
        let profil = source
            .find("let profile_id = if let Some(id) = existing_id")
            .expect("la resolution du profil a disparu");
        let licence = source
            .find("set_account_premium")
            .expect("l'ecriture de premium a disparu");
        assert!(
            profil < licence,
            "la licence s'ecrit AVANT que le profil ne soit connu : le garde \
             de propriete ne peut alors rien decider"
        );
    }
}

#[cfg(test)]
mod validation_licence_3906 {
    use super::{MotifValidation, corps_validation, message_statut_distant};
    use axum::http::StatusCode;
    use serde_json::json;

    /// Recensement exhaustif des motifs.
    ///
    /// Le `match` ci-dessous est la garde : ajouter un dixième motif sans
    /// l'inscrire ici **ne compile plus**. Sans lui, une dixième sortie
    /// pourrait naître sans que le moindre essai la voie — c'est exactement
    /// la famille de défaut que ce ticket corrige.
    fn tous_les_motifs() -> Vec<MotifValidation> {
        use MotifValidation::*;
        let recensement = vec![
            AucuneCle,
            RequeteEchouee,
            PointAccesAbsent,
            StatutDistant,
            ReponseIllisible,
            SansVerdict,
            RefusTransitoire,
            Expiree,
            Confirmee,
        ];
        for motif in &recensement {
            match motif {
                AucuneCle | RequeteEchouee | PointAccesAbsent | StatutDistant
                | ReponseIllisible | SansVerdict | RefusTransitoire | Expiree | Confirmee => {}
            }
        }
        recensement
    }

    #[test]
    fn les_neuf_sorties_portent_un_motif_distinct() {
        let motifs = tous_les_motifs();
        assert_eq!(
            motifs.len(),
            9,
            "le recensement doit couvrir les neuf sorties de la route"
        );
        let mut vus = std::collections::BTreeSet::new();
        for motif in &motifs {
            assert!(
                vus.insert(motif.code()),
                "deux sorties partagent le code « {} » : un client ne peut pas les distinguer, \
                 et le statut HTTP vaut 200 pour les deux",
                motif.code()
            );
        }
    }

    #[test]
    fn les_trois_sorties_cached_se_distinguent_par_leur_motif() {
        let cached = [
            MotifValidation::PointAccesAbsent,
            MotifValidation::SansVerdict,
            MotifValidation::RefusTransitoire,
        ];
        let codes: std::collections::BTreeSet<_> = cached.iter().map(|m| m.code()).collect();
        for motif in cached {
            assert_eq!(
                motif.status(),
                "cached",
                "{:?} doit garder le `status` que les clients déployés lisent",
                motif
            );
        }
        assert_eq!(
            codes.len(),
            3,
            "les trois sorties « cached » — 404 distant, verdict absent, refus transitoire — \
             doivent porter TROIS codes : sans cela le client ne peut pas dire à l'utilisateur \
             lequel des trois lui est arrivé. Codes obtenus : {codes:?}"
        );
    }

    #[test]
    fn les_trois_sorties_error_se_distinguent_par_leur_motif() {
        let erreurs = [
            MotifValidation::RequeteEchouee,
            MotifValidation::StatutDistant,
            MotifValidation::ReponseIllisible,
        ];
        let codes: std::collections::BTreeSet<_> = erreurs.iter().map(|m| m.code()).collect();
        for motif in erreurs {
            assert_eq!(
                motif.status(),
                "error",
                "{:?} doit garder le `status` que les clients déployés lisent",
                motif
            );
        }
        assert_eq!(
            codes.len(),
            3,
            "les trois sorties « error » — requête qui n'aboutit pas, statut distant, corps \
             illisible — doivent porter TROIS codes. Codes obtenus : {codes:?}"
        );
    }

    #[test]
    fn seule_une_licence_confirmee_rend_ok_vrai() {
        for motif in tous_les_motifs() {
            let attendu = motif == MotifValidation::Confirmee;
            assert_eq!(
                motif.succes(),
                attendu,
                "`ok` doit valoir {attendu} pour {motif:?} : c'est le seul champ que le client \
                 peut tester sans connaître l'histoire de cette route"
            );
        }
    }

    #[test]
    fn le_corps_porte_les_trois_champs_sans_perdre_le_reste() {
        let corps = corps_validation(
            MotifValidation::StatutDistant,
            json!({
                "tier": "premium",
                "message": "Server returned 429 Too Many Requests",
                "cached": true,
                "upstream_status": 429,
            }),
        );
        assert_eq!(
            corps["status"],
            json!("error"),
            "le `status` historique est conservé"
        );
        assert_eq!(
            corps["reason"],
            json!("upstream_error"),
            "`reason` est le seul discriminant stable de cette sortie"
        );
        assert_eq!(
            corps["ok"],
            json!(false),
            "un statut distant d'erreur ne pose aucun palier"
        );
        assert_eq!(
            corps["upstream_status"],
            json!(429),
            "429 doit être lisible en NOMBRE : sans lui le client doit analyser une phrase anglaise"
        );
        assert_eq!(
            corps["tier"],
            json!("premium"),
            "le corps d'origine doit survivre"
        );
        assert_eq!(
            corps["cached"],
            json!(true),
            "le corps d'origine doit survivre"
        );
    }

    #[test]
    fn upstream_status_est_toujours_present_meme_sans_aller_retour() {
        for motif in tous_les_motifs() {
            let corps = corps_validation(motif, json!({ "tier": "free" }));
            assert!(
                corps.get("upstream_status").is_some(),
                "`upstream_status` doit exister pour {motif:?}, fût-il nul : un client typé ne \
                 doit pas avoir à distinguer « champ absent » de « pas d'aller-retour »"
            );
        }
        let sans_aller_retour = corps_validation(MotifValidation::RequeteEchouee, json!({}));
        assert!(
            sans_aller_retour["upstream_status"].is_null(),
            "aucune requête n'a abouti : il n'y a pas de statut distant à annoncer"
        );
    }

    #[test]
    fn le_motif_fait_foi_sur_un_corps_qui_se_contredit() {
        let corps = corps_validation(
            MotifValidation::RefusTransitoire,
            json!({ "status": "validated", "ok": true, "reason": "confirmed" }),
        );
        assert_eq!(
            corps["status"],
            json!("cached"),
            "le motif écrase un `status` menteur"
        );
        assert_eq!(corps["ok"], json!(false), "le motif écrase un `ok` menteur");
        assert_eq!(
            corps["reason"],
            json!("rejected_transient"),
            "le motif écrase un `reason` menteur"
        );
    }

    #[test]
    fn le_message_du_statut_distant_garde_le_prefixe_que_le_client_analyse() {
        let plafond = message_statut_distant(StatusCode::TOO_MANY_REQUESTS);
        assert!(
            plafond.starts_with("Server returned 429"),
            "le client web déployé (tune-web-client v0.9.146, `statutDistantDepuisMessage`) \
             extrait le code distant par `^Server returned (\\d{{3}})` : reformuler cette phrase \
             lui retire la distinction 429/502 sans un seul rouge. Message obtenu : « {plafond} »"
        );
        let panne = message_statut_distant(StatusCode::BAD_GATEWAY);
        assert!(
            panne.starts_with("Server returned 502"),
            "même contrat pour une panne du serveur distant. Message obtenu : « {panne} »"
        );
    }

    /// Le corps de la route, découpé dans son propre source.
    ///
    /// Les aiguilles sont construites à l'exécution : écrites en clair, ce
    /// module se compterait lui-même.
    fn corps_de_la_route() -> String {
        let source = include_str!("cloud.rs");
        let entete = format!("async fn license{}validate(", "_");
        let debut = source
            .find(&entete)
            .expect("la route `license_validate` doit exister dans ce fichier");
        let reste = &source[debut..];
        let fin = reste
            .find("\n}\n")
            .expect("la route doit se fermer sur une accolade en colonne 0");
        reste[..fin].to_string()
    }

    /// Garde de SITE : aucune sortie de la route ne doit répondre sans motif.
    ///
    /// Une dixième sortie écrite à la main — `Json(json!(…)).into_response()` —
    /// rendrait un corps sans `reason`, sans `ok` et sans `upstream_status`,
    /// avec le même HTTP 200 que toutes les autres. Invisible à l'œil, et
    /// invisible aux essais de corps ci-dessus, qui n'exercent que la fonction
    /// pure. C'est cette garde-là qui la voit.
    #[test]
    fn aucune_sortie_de_license_validate_ne_repond_sans_motif() {
        let corps = corps_de_la_route();
        let echappatoire = format!(".into{}response()", "_");
        assert_eq!(
            corps.matches(&echappatoire).count(),
            0,
            "une sortie de `license_validate` construit sa réponse à la main au lieu de passer \
             par `reponse_validation` : elle rendra un 200 sans `reason`, et le client ne pourra \
             pas la distinguer des huit autres (#3906)"
        );
        let fabrique = format!("reponse{}validation(", "_");
        assert_eq!(
            corps.matches(&fabrique).count(),
            9,
            "la route doit avoir exactement neuf sorties, toutes passées par `reponse_validation`"
        );
        for motif in tous_les_motifs() {
            let nom = format!("{}::{:?}", "MotifValidation", motif);
            assert!(
                corps.contains(&nom),
                "le motif {nom} n'est employé par AUCUNE sortie de la route : soit la sortie a \
                 disparu, soit elle répond désormais sous un autre motif"
            );
        }
    }
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

/// Corps de `POST /cloud/library-sync/reconcile`.
#[derive(serde::Deserialize, Default)]
struct ReconcileBody {
    /// Mettre reellement les suppressions en file. Par defaut `false` : on
    /// regarde le plan avant de l'executer.
    #[serde(default)]
    apply: bool,
    /// Inclure les pistes. Par defaut `false` : 235 pages sous un plafond de
    /// 60 requetes par minute, pour un ecart mesure de -1.
    #[serde(default)]
    tracks: bool,
}

/// Le mode que le corps de la requete demande.
///
/// `Mode::ABlanc` sauf demande EXPLICITE. Extrait en fonction PURE parce
/// qu'une garde qui lit la source ne prouve pas grand-chose : elle se fait
/// aveugler des qu'un module de test etranger est pose plus haut dans le
/// fichier (mesure le 12/09/2026 — #3906 a insere `validation_licence_3906`
/// AVANT ce point d'appel, et la coupe a `#[cfg(test)]` amputait tout le
/// reste). Ici, le test APPELLE cette fonction : plus rien a grepper.
///
/// Le type — et non un second `bool` colle a `body.tracks` — interdit en
/// outre d'echanger les deux arguments a l'appel de `reconcilier()`.
fn mode_demande(body: &ReconcileBody) -> tune_core::cloud::library_reconcile::Mode {
    use tune_core::cloud::library_reconcile::Mode;
    if body.apply {
        Mode::Appliquer
    } else {
        Mode::ABlanc
    }
}

/// POST /cloud/library-sync/reconcile — effacer du cloud ce que le serveur ne
/// possede plus.
///
/// A BLANC par defaut. Le rapport dit alors ce qui SERAIT supprime, et le
/// journal porte le plan (`cloud_library_reconcile_plan`). Passer
/// `{"apply": true}` met les suppressions en file ; c'est le passage de
/// synchro suivant qui les pousse.
///
/// Manuelle et non automatique, deliberement : declencher une suppression de
/// masse au demarrage sans l'avoir vue tourner une fois serait imprudent.
async fn library_sync_reconcile(
    State(state): State<AppState>,
    body: Option<Json<ReconcileBody>>,
) -> impl IntoResponse {
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::CloudBackup,
    )
    .await
    {
        return resp;
    }

    let Json(body) = body.unwrap_or_default();

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

    let mode = mode_demande(&body);

    let rapport = tune_core::cloud::library_reconcile::reconcilier(
        &state.backend,
        &state.http_client,
        tune_core::cloud::library_reconcile::CLOUD_LIBRARY_API,
        &server_id,
        &token,
        body.tracks,
        mode,
    )
    .await;

    Json(json!({
        "dry_run": rapport.a_blanc,
        "artists": rapport.artistes_orphelins,
        "albums": rapport.albums_orphelins,
        "tracks": rapport.pistes_orphelines,
        "total": rapport.total(),
        "refused": rapport.refus,
        "pending_after": tune_core::cloud::library_sync::pending_count(&state.backend),
    }))
    .into_response()
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

#[cfg(test)]
mod reconcile_tests {
    use super::*;

    /// La propriete qui compte : un appel sans corps, ou avec un corps qui ne
    /// dit rien, ne doit RIEN supprimer. La route passe `!body.apply` au
    /// module, donc `apply = false` signifie « a blanc ».
    ///
    /// Si ce booleen s'inverse un jour, un simple `curl -X POST` sur la route
    /// viderait le catalogue en ligne de tout ce que le serveur ne possede
    /// plus — sans que personne l'ait demande.
    #[test]
    fn la_reconciliation_est_a_blanc_par_defaut() {
        assert!(
            !ReconcileBody::default().apply,
            "le defaut doit etre `a blanc`"
        );

        let vide: ReconcileBody = serde_json::from_str("{}").expect("corps vide");
        assert!(!vide.apply, "un corps vide doit rester `a blanc`");
        assert!(
            !vide.tracks,
            "un corps vide ne doit pas parcourir les pistes"
        );

        let demande: ReconcileBody =
            serde_json::from_str(r#"{"apply":true}"#).expect("corps explicite");
        assert!(
            demande.apply,
            "seule une demande EXPLICITE doit mettre en file"
        );
    }

    /// La source de ce fichier, amputee de CE module de test.
    ///
    /// La coupe doit ignorer le module de test lui-meme : sinon la chaine
    /// cherchee apparait dans sa propre assertion et la garde se satisfait
    /// toute seule. Contre-epreuve du 04/09/2026 : sans cette coupe, retirer
    /// la negation dans la route laissait le test VERT.
    ///
    /// 🔴 Elle coupait au PREMIER `#[cfg(test)]` du fichier. Mesure le
    /// 12/09/2026 : #3906 a depuis pose `validation_licence_3906` a la ligne
    /// 1396, soit AVANT le gestionnaire de reconciliation — la tranche
    /// inspectee s'arretait donc bien avant lui, et la garde du mode par
    /// defaut rougissait sans qu'aucune regression n'ait eu lieu. La coupe se
    /// fait desormais sur CE module, nomme, quel que soit le nombre de
    /// modules de test poses plus haut.
    fn source_hors_tests() -> &'static str {
        let source = include_str!("cloud.rs");
        match source.find("mod reconcile_tests") {
            Some(i) => &source[..i],
            None => panic!("`mod reconcile_tests` introuvable — coupe impossible"),
        }
    }

    #[test]
    fn la_route_de_reconciliation_est_montee() {
        let source = source_hors_tests();
        assert!(
            source.contains(r#".route("/library-sync/reconcile", post(library_sync_reconcile))"#),
            "la route /library-sync/reconcile doit etre montee sur le routeur"
        );

        // Et que le gestionnaire decide bien son mode par `mode_demande` —
        // dont le test ci-dessous eprouve le defaut. Sans ce lien, la
        // fonction pourrait etre juste et le gestionnaire ne pas s'en servir.
        assert!(
            source.contains("let mode = mode_demande(&body);"),
            "le gestionnaire doit prendre son mode de `mode_demande`"
        );
    }

    /// 🔴 La propriete qui compte cote route : sans demande EXPLICITE, le mode
    /// est `ABlanc`. Eprouvee en APPELANT la production, pas en lisant sa
    /// source.
    ///
    /// Ce que ce test ne prouve pas : que le mode a blanc n'ecrit rien. Ca,
    /// c'est `tune-core/tests/reconciliation_a_blanc_2373.rs` qui l'exerce,
    /// en faisant agir `reconcilier()` contre un cloud simule puis en
    /// relisant `sync_changelog`.
    #[test]
    fn sans_demande_explicite_le_mode_est_a_blanc() {
        use tune_core::cloud::library_reconcile::Mode;

        assert_eq!(mode_demande(&ReconcileBody::default()), Mode::ABlanc);

        let vide: ReconcileBody = serde_json::from_str("{}").expect("corps vide");
        assert_eq!(
            mode_demande(&vide),
            Mode::ABlanc,
            "un `curl -X POST` sans corps doit simuler, jamais supprimer"
        );

        let autre_chose: ReconcileBody =
            serde_json::from_str(r#"{"tracks":true}"#).expect("corps sans `apply`");
        assert_eq!(
            mode_demande(&autre_chose),
            Mode::ABlanc,
            "un corps qui ne dit rien d`apply` doit rester a blanc"
        );

        let demande: ReconcileBody =
            serde_json::from_str(r#"{"apply":true}"#).expect("demande explicite");
        assert_eq!(
            mode_demande(&demande),
            Mode::Appliquer,
            "seule une demande EXPLICITE doit mettre en file"
        );
    }
}
