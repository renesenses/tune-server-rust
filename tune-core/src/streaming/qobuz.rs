use std::time::Duration;

use reqwest::Client;
use tracing::{debug, info, warn};

use super::traits::*;
use crate::TuneError;

const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";
const API_PROXY: &str = "https://mozaiklabs.fr/qobuz-api";
const REMOTE_CONFIG_URL: &str = "https://mozaiklabs.fr/storage/api/v1/streaming-config.json";

pub struct QobuzService {
    client: Client,
    app_id: String,
    app_secret: String,
    user_auth_token: Option<String>,
    username: Option<String>,
    subscription: Option<String>,
    /// Endpoint order: `false` (default) = direct Qobuz API first with the
    /// mozaiklabs proxy as fallback; `true` = proxy first with direct as
    /// fallback (founder accounts, signalled by the cloud license via the
    /// optional `qobuz_proxy_first` field).
    proxy_first: bool,
    stored_username: Option<String>,
    /// Password from the interactive login, kept for `auto_relogin` — **memory
    /// only, never persisted**. `save_tokens` deliberately omits it; see the
    /// note there.
    stored_password: Option<String>,
    enabled_override: Option<bool>,
    /// Last auto-relogin attempt, successful or not — see `auto_relogin`.
    last_relogin_attempt: Option<std::time::Instant>,
    /// Set when `restore_tokens` read a pre-fix row carrying `stored_password`.
    needs_token_rewrite: bool,
    /// Set when Qobuz rejected the token and no relogin was possible — the
    /// session is over and the persisted row is worthless.
    session_expired: bool,
}

/// (primary, fallback) API bases for the given endpoint order.
///
/// `proxy_first == false` (default, all users): direct Qobuz API first,
/// mozaiklabs proxy as fallback. `proxy_first == true` (founder account):
/// proxy first, direct as fallback.
fn endpoint_order(proxy_first: bool) -> (&'static str, &'static str) {
    if proxy_first {
        (API_PROXY, API_BASE)
    } else {
        (API_BASE, API_PROXY)
    }
}

/// Error from a single API attempt against one base URL.
#[derive(Debug)]
enum AttemptError {
    /// Network-level failure (timeout, DNS, connect) — eligible for fallback.
    Network(String),
    /// HTTP error status. 5xx is eligible for fallback; 4xx is final.
    Http { status: u16, body: String },
    /// Body of a successful response failed to parse — final, no fallback.
    Json(String),
}

impl AttemptError {
    /// Whether the other endpoint should be tried (network error or 5xx).
    fn transient(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Http { status, .. } => *status >= 500,
            Self::Json(_) => false,
        }
    }
}

impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "{e}"),
            Self::Http { status, body } => write!(f, "{status} {body}"),
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

/// Log the primary-endpoint failure that triggers the fallback, keeping the
/// historical proxy-first event names and their direct-first mirrors.
fn log_fallback(proxy_first: bool, path: &str, err: &AttemptError) {
    match (proxy_first, err) {
        (true, AttemptError::Http { status, .. }) => {
            info!(path, status, "qobuz_proxy_5xx_trying_direct");
        }
        (true, _) => info!(path, error = %err, "qobuz_proxy_failed_trying_direct"),
        (false, AttemptError::Http { status, .. }) => {
            info!(path, status, "qobuz_direct_5xx_trying_proxy");
        }
        (false, _) => info!(path, error = %err, "qobuz_direct_failed_trying_proxy"),
    }
}

/// Traduit le type de favori du client (pluriel) en paramètre attendu par
/// l'API Qobuz.
fn favorite_key(fav_type: &str) -> Result<&'static str, TuneError> {
    match fav_type {
        "tracks" => Ok("track_ids"),
        "albums" => Ok("album_ids"),
        "artists" => Ok("artist_ids"),
        _ => Err(format!("unknown favorite type: {fav_type}").into()),
    }
}

/// Trace le résultat d'une écriture de favori chez Qobuz.
///
/// Sans cette trace, un favori qui n'arrive jamais dans l'app Qobuz ne laisse
/// aucune empreinte exploitable : l'appel HTTP réussit, le cœur bascule dans
/// Tune, et le rapport du testeur se réduit à « ça ne marche pas ». On veut
/// pouvoir répondre depuis ses logs (retour Fabien, 08/08/2026).
fn log_favorite_result(
    op: &str,
    fav_type: &str,
    item_id: &str,
    res: &Result<serde_json::Value, String>,
) {
    match res {
        Ok(body) => info!(op, fav_type, item_id, response = %body, "qobuz_favorite_ok"),
        Err(e) => warn!(op, fav_type, item_id, error = %e, "qobuz_favorite_failed"),
    }
}

impl QobuzService {
    /// Écrire un favori exige le jeton utilisateur : l'app_id seul identifie
    /// l'application, pas le compte. Sans jeton, Qobuz accepte la requête sans
    /// rien enregistrer — un succès en trompe-l'œil. On refuse avant d'émettre.
    fn require_user_token(&self, op: &str) -> Result<(), TuneError> {
        if self.user_auth_token.is_none() {
            warn!(op, "qobuz_favorite_no_user_token");
            return Err("session Qobuz non authentifiée : reconnecte le compte Qobuz".into());
        }
        Ok(())
    }

    pub fn new(app_id: String, app_secret: String) -> Self {
        Self {
            client: crate::http::client::builder()
                .timeout(Duration::from_secs(45))
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
                .build()
                .unwrap_or_else(|_| Client::new()),
            app_id,
            app_secret,
            user_auth_token: None,
            username: None,
            subscription: None,
            proxy_first: false,
            stored_username: None,
            stored_password: None,
            enabled_override: None,
            last_relogin_attempt: None,
            needs_token_rewrite: false,
            session_expired: false,
        }
    }

    /// Set the endpoint order: `true` = proxy first (founder account, from the
    /// cloud license `qobuz_proxy_first` flag), `false` = direct first.
    pub fn set_proxy_first(&mut self, proxy_first: bool) {
        if proxy_first != self.proxy_first {
            info!(proxy_first, "qobuz_endpoint_order_changed");
        }
        self.proxy_first = proxy_first;
    }

    async fn refresh_credentials(&mut self) {
        match self.client.get(REMOTE_CONFIG_URL).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let qobuz = &data["qobuz"];
                    if let (Some(id), Some(secret)) =
                        (qobuz["app_id"].as_str(), qobuz["app_secret"].as_str())
                    {
                        info!(old_id = %&self.app_id, new_id = %id, "qobuz_credentials_refreshed");
                        self.app_id = id.to_string();
                        self.app_secret = secret.to_string();
                    }
                }
            }
            _ => info!("qobuz_remote_config_unavailable"),
        }
    }

    /// GET against the primary endpoint for the configured order, falling back
    /// to the other endpoint on a network error or 5xx (symmetric fallback).
    async fn api_get(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let (primary, fallback) = endpoint_order(self.proxy_first);
        match self.api_get_at(primary, path, params).await {
            Ok(v) => Ok(v),
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, path, &err);
                self.api_get_at(fallback, path, params).await.map_err(|e| {
                    info!(path, error = %e, "qobuz_fallback_api_error");
                    format!("qobuz {path}: {e}")
                })
            }
            Err(err) => {
                info!(path, error = %err, "qobuz_api_error");
                Err(format!("qobuz {path}: {err}"))
            }
        }
    }

    /// One GET attempt against a specific API base.
    async fn api_get_at(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, AttemptError> {
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("app_id", app_id));

        let mut req = self
            .client
            .get(&url)
            .query(&query)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }

        resp.json()
            .await
            .map_err(|e| AttemptError::Json(e.to_string()))
    }

    /// Fetch all pages from a paginated Qobuz endpoint.
    ///
    /// `path` / `base_params` define the request. `items_key` is the top-level
    /// JSON key that wraps the `items` array (e.g. "tracks", "albums", "artists").
    /// The Qobuz API caps each page at 50 items regardless of the requested limit.
    async fn api_get_all_pages(
        &self,
        path: &str,
        base_params: &[(&str, &str)],
        items_key: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        const PAGE_SIZE: usize = 50;
        let mut all_items: Vec<serde_json::Value> = Vec::new();
        let mut offset: usize = 0;

        loop {
            let offset_str = offset.to_string();
            let limit_str = PAGE_SIZE.to_string();
            let mut params: Vec<(&str, &str)> = base_params.to_vec();
            params.push(("limit", &limit_str));
            params.push(("offset", &offset_str));

            let data = self.api_get(path, &params).await?;

            let items = data[items_key]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();

            let count = items.len();
            all_items.extend(items);

            let total = data[items_key]["total"].as_u64().unwrap_or(0) as usize;

            debug!(
                path,
                items_key,
                offset,
                count,
                total,
                accumulated = all_items.len(),
                "qobuz_paginate"
            );

            // Diagnostic for the "favorites empty while playlists load" reports
            // (Stéphane): on the FIRST page, surface at INFO the counts so a normal
            // tester log tells us whether Qobuz returned nothing (total==0 → an
            // API/account issue) or returned rows we then dropped (total>0 but
            // count==0 → a response-shape/mapping mismatch on our side). When the
            // items_key sub-object is entirely absent we log the top-level keys
            // the response DID carry, to catch Qobuz nesting the data elsewhere.
            if offset == 0 {
                let key_present = data.get(items_key).map(|v| !v.is_null()).unwrap_or(false);
                if total == 0 && count == 0 {
                    let top_keys: Vec<&str> = data
                        .as_object()
                        .map(|m| m.keys().map(String::as_str).collect())
                        .unwrap_or_default();
                    warn!(
                        path,
                        items_key,
                        key_present,
                        response_keys = ?top_keys,
                        "qobuz_favorites_empty"
                    );
                } else {
                    info!(path, items_key, count, total, "qobuz_favorites_first_page");
                }
            }

            offset += count;

            // Stop when we got fewer items than a full page, or we've reached the total
            if count < PAGE_SIZE || offset >= total {
                break;
            }
        }

        Ok(all_items)
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["performer"]["name"]
                .as_str()
                .or_else(|| item["artist"]["name"].as_str())
                .unwrap_or("")
                .into(),
            album: album["title"].as_str().map(Into::into),
            album_id: album["id"]
                .as_str()
                .map(Into::into)
                .or_else(|| album["id"].as_u64().map(|id| id.to_string())),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_path: album["image"]["large"].as_str().map(Into::into),
            track_number: item["track_number"].as_u64().map(|n| n as u32),
            disc_number: item["media_number"].as_u64().map(|n| n as u32),
            explicit: item["parental_warning"].as_bool().unwrap_or(false),
            isrc: item["isrc"].as_str().map(Into::into),
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate: item["maximum_sampling_rate"]
                    .as_f64()
                    .map(|r| (r * 1000.0) as u32)
                    .unwrap_or(44100),
                bit_depth: item["maximum_bit_depth"]
                    .as_u64()
                    .map(|b| b as u16)
                    .unwrap_or(16),
                bitrate: None,
                channels: 2,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"]
                .as_str()
                .map(Into::into)
                .or_else(|| item["id"].as_u64().map(|id| id.to_string()))
                .unwrap_or_default(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str().unwrap_or("").into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_path: item["image"]["large"].as_str().map(Into::into),
            year: item["released_at"]
                .as_u64()
                .map(|ts| 1970 + (ts / 31_536_000) as u32)
                .or_else(|| {
                    item["release_date_original"]
                        .as_str()
                        .and_then(|d| d.get(..4)?.parse().ok())
                }),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            quality: item["maximum_bit_depth"].as_u64().map(|bd| StreamQuality {
                codec: "FLAC".into(),
                sample_rate: item["maximum_sampling_rate"]
                    .as_f64()
                    .map(|r| (r * 1000.0) as u32)
                    .unwrap_or(44100),
                bit_depth: bd as u16,
                bitrate: None,
                channels: 2,
            }),
        }
    }

    /// Map a Qobuz featured/editorial playlist item to StreamPlaylist.
    fn map_featured_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| item["id"].as_str().map(Into::into))
                .unwrap_or_default(),
            name: item["name"].as_str().unwrap_or("").into(),
            description: item["description"].as_str().map(Into::into),
            cover_path: item["image_rectangle"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .or_else(|| {
                    item["images150"]
                        .as_array()
                        .and_then(|a| a.first())?
                        .as_str()
                })
                .or_else(|| item["images"].as_array().and_then(|a| a.first())?.as_str())
                .map(Into::into),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            owner: item["owner"]["name"].as_str().map(Into::into),
        }
    }

    /// One login attempt against a specific API base.
    async fn login_at(
        &self,
        base: &str,
        username: &str,
        password: &str,
    ) -> Result<serde_json::Value, AttemptError> {
        let resp = self
            .client
            .post(format!("{base}/user/login"))
            .query(&[("app_id", self.app_id.as_str())])
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }

        resp.json()
            .await
            .map_err(|e| AttemptError::Json(e.to_string()))
    }

    /// Login following the configured endpoint order. Direct-first by default so
    /// user credentials never transit through the VPS unless the direct API is
    /// unreachable (network error or 5xx) — proxy-first for founder accounts.
    async fn login_internal(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<AuthStatus, String> {
        self.refresh_credentials().await;

        let (primary, fallback) = endpoint_order(self.proxy_first);
        let data = match self.login_at(primary, username, password).await {
            Ok(d) => d,
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, "/user/login", &err);
                self.login_at(fallback, username, password)
                    .await
                    .map_err(|e| {
                        info!(error = %e, "qobuz_login_failed");
                        format!("qobuz login: {e}")
                    })?
            }
            Err(err) => {
                info!(error = %err, "qobuz_login_failed");
                return Err(format!("qobuz login: {err}"));
            }
        };

        self.user_auth_token = data["user_auth_token"].as_str().map(Into::into);
        self.username = data["user"]["display_name"].as_str().map(Into::into);
        self.subscription = data["user"]["credential"]["label"].as_str().map(Into::into);
        // A fresh session clears the expiry flag, whether this login came from
        // the user or from `auto_relogin`.
        self.session_expired = false;

        info!(username = ?self.username, "qobuz_authenticated");
        Ok(self.auth_status_internal())
    }

    fn auth_status_internal(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.user_auth_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            ..Default::default()
        }
    }

    async fn auto_relogin(&mut self) -> bool {
        // Cooldown : pendant une coupure (proxy mozaiklabs down + direct
        // bloqué), chaque appel en échec relançait un login complet — une
        // tempête de relogins toutes les ~2 s qui ressemble à du trafic de
        // bot et fait rate-limiter l'IP par Akamai (403 en cascade, .18 le
        // 28/07). Une tentative par minute suffit largement à récupérer dès
        // que le réseau revient.
        const RELOGIN_COOLDOWN: Duration = Duration::from_secs(60);
        if let Some(t) = self.last_relogin_attempt
            && t.elapsed() < RELOGIN_COOLDOWN
        {
            debug!("qobuz_auto_relogin_cooldown");
            return false;
        }
        self.last_relogin_attempt = Some(std::time::Instant::now());
        if let (Some(u), Some(p)) = (self.stored_username.clone(), self.stored_password.clone()) {
            info!("qobuz_auto_relogin");
            self.login_internal(&u, &p).await.is_ok()
        } else {
            false
        }
    }

    /// POST against the primary endpoint for the configured order, falling back
    /// to the other endpoint on a network error or 5xx (same order as api_get).
    async fn api_post(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let (primary, fallback) = endpoint_order(self.proxy_first);
        match self.api_post_at(primary, path, params).await {
            Ok(v) => Ok(v),
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, path, &err);
                self.api_post_at(fallback, path, params)
                    .await
                    .map_err(|e| format!("qobuz {path}: {e}"))
            }
            Err(err) => Err(format!("qobuz {path}: {err}")),
        }
    }

    /// One POST attempt against a specific API base.
    async fn api_post_at(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, AttemptError> {
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        // Qobuz's Akamai edge rejects a body-less POST with HTTP 411 (Length
        // Required). Send the parameters as an `application/x-www-form-urlencoded`
        // body instead of a query string so the request carries a Content-Length.
        let mut form: Vec<(&str, &str)> = params.to_vec();
        form.push(("app_id", app_id));
        if let Some(ref token) = self.user_auth_token {
            form.push(("user_auth_token", token.as_str()));
        }

        let mut req = self
            .client
            .post(&url)
            .form(&form)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }
        resp.json()
            .await
            .or_else(|_| Ok(serde_json::json!({"ok": true})))
    }

    /// Determine the best format_id given the user's subscription level.
    /// "Studio" / "HiFi" subscriptions max out at CD quality (format_id 6).
    /// "Sublime" / "Sublime+" can access Hi-Res (format_id 27).
    fn best_format_id_for_subscription(&self) -> &str {
        // Always try Hi-Res (27) first — the caller falls back to CD (6) if
        // the subscription doesn't support it. This avoids silently
        // downsampling Hi-Res content for users whose subscription field
        // isn't detected as "sublime".
        "27"
    }

    /// Low-level fetch of a track streaming URL with a specific format_id.
    async fn fetch_track_url(&self, track_id: &str, format_id: &str) -> Result<StreamUrl, String> {
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let timestamp = format!("{}.{}", dur.as_secs(), dur.subsec_millis());

        let sig_input = format!(
            "trackgetFileUrlformat_id{format_id}intentstreamtrack_id{track_id}{timestamp}{}",
            self.app_secret
        );
        let sig = md5_hex(&sig_input);

        info!(track_id, format_id, timestamp = %timestamp, sig = %sig, "qobuz_get_file_url");

        let data = self
            .api_get(
                "/track/getFileUrl",
                &[
                    ("track_id", track_id),
                    ("format_id", format_id),
                    ("intent", "stream"),
                    ("request_ts", &timestamp),
                    ("request_sig", &sig),
                ],
            )
            .await?;

        // Qobuz returns a 30-second preview (audio/mpeg, URL carries `range=20-30`,
        // `"sample": true`) instead of an error when the requested format_id is
        // above the subscription's entitlement — e.g. asking for Hi-Res (27) on a
        // "streaming-studio" CD-max plan. Accepting it silently makes playback cut
        // at exactly 30s (DLNA renderers download the whole preview, then stall;
        // Qobuz → DMP-A8, .15). Reject a sample so `get_track_url` falls down the
        // quality ladder to a format the subscription can actually stream in full.
        if data["sample"].as_bool().unwrap_or(false) {
            info!(track_id, format_id, "qobuz_sample_preview_rejected");
            return Err(format!(
                "qobuz returned a 30s sample for format_id {format_id} — not entitled at this quality"
            ));
        }

        let url = data["url"].as_str().ok_or("no url")?.to_string();
        let mime = data["mime_type"]
            .as_str()
            .unwrap_or("audio/flac")
            .to_string();
        let sample_rate = data["sampling_rate"]
            .as_f64()
            .map(|r| (r * 1000.0) as u32)
            .unwrap_or(44100);
        let bit_depth = data["bit_depth"].as_u64().map(|b| b as u16).unwrap_or(16);

        Ok(StreamUrl {
            url,
            mime_type: mime,
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate,
                bit_depth,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
        })
    }

    fn map_genre(item: &serde_json::Value) -> StreamGenre {
        // Qobuz returns subgenresCount (integer) rather than a subgenres array
        // at the /genre/list level. Fall back to checking the subgenres array
        // (returned by /genre/get) or slug depth as a heuristic.
        let has_children = item["subgenresCount"]
            .as_u64()
            .map(|n| n > 0)
            .or_else(|| item["subgenres"].as_array().map(|a| !a.is_empty()))
            .unwrap_or_else(|| {
                // Top-level genres (slug without '/') typically have children
                item["slug"]
                    .as_str()
                    .map(|s| !s.contains('/'))
                    .unwrap_or(false)
            });

        // Qobuz image can be a string or an object {"large": "...", "small": "..."}
        let image_url = item["image"]
            .as_str()
            .map(String::from)
            .or_else(|| item["image"]["large"].as_str().map(String::from));

        StreamGenre {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children,
            image_url,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["image"]["large"].as_str().map(Into::into),
            bio: item["biography"]["content"]
                .as_str()
                .or_else(|| item["biography"]["summary"].as_str())
                .map(Self::strip_html_tags)
                .filter(|s| !s.is_empty()),
        }
    }

    /// Strip HTML tags from Qobuz editorial text (biography content is HTML).
    fn strip_html_tags(s: &str) -> String {
        let re = regex::Regex::new(r"<[^>]+>").unwrap();
        re.replace_all(s, "").trim().to_string()
    }
}

#[async_trait::async_trait]
impl StreamingService for QobuzService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "qobuz"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(!self.app_id.is_empty())
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        let username = credentials["username"]
            .as_str()
            .ok_or("username required")?;
        let password = credentials["password"]
            .as_str()
            .ok_or("password required")?;

        self.stored_username = Some(username.to_string());
        self.stored_password = Some(password.to_string());

        Ok(self.login_internal(username, password).await?)
    }

    async fn auth_status(&self) -> AuthStatus {
        self.auth_status_internal()
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.user_auth_token = None;
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let data = self
            .api_get(
                "/catalog/search",
                &[("query", query), ("limit", &limit.to_string())],
            )
            .await?;

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        let artists = data["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();

        Ok(SearchResults {
            tracks,
            albums,
            artists,
            playlists: vec![],
        })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        let data = self
            .api_get("/track/get", &[("track_id", track_id)])
            .await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        if self.user_auth_token.is_none() {
            return Err(TuneError::Streaming(
                "Qobuz session expired — please reconnect in Settings → Streaming Services".into(),
            ));
        }

        let format_id = match quality {
            Some("hires") => "27",
            Some("cd") => "6",
            Some("mp3") => "5",
            _ => self.best_format_id_for_subscription(),
        };

        match self.fetch_track_url(track_id, format_id).await {
            Ok(stream_url) => Ok(stream_url),
            Err(e) => {
                // Fall down the quality ladder to the next-lower format the track
                // is actually offered in: 24/192 (27) → 24/96 (7) → CD (6).
                // Falling straight from 27 to 6 dropped Sublime (Hi-Res) users to
                // lossy-CD on the many tracks Qobuz only offers in 24/96, instead
                // of the hi-res they're entitled to (Yves). CD (6) and MP3 (5)
                // have no lossless format below them.
                let ladder: &[&str] = match format_id {
                    "27" => &["7", "6"],
                    "7" => &["6"],
                    _ => &[],
                };
                for &fid in ladder {
                    info!(
                        track_id,
                        from = format_id,
                        to = fid,
                        "qobuz_format_fallback"
                    );
                    if let Ok(url) = self.fetch_track_url(track_id, fid).await {
                        return Ok(url);
                    }
                }
                Err(e.into())
            }
        }
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        let data = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        // Qobuz album/get returns album metadata at the top level while
        // individual track items inside tracks.items do NOT carry an
        // "album" sub-object.  Extract the album-level title, image and
        // id so we can inject them into each mapped track.
        let album_title = data["title"].as_str().map(String::from);
        let album_cover = data["image"]["large"]
            .as_str()
            .or_else(|| data["image"]["small"].as_str())
            .map(String::from);
        let album_id_val = data["id"]
            .as_str()
            .map(String::from)
            .or_else(|| data["id"].as_u64().map(|id| id.to_string()));

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let mut t = Self::map_track(item);
                        // Inject album-level metadata when the track lacks it
                        if t.album.is_none() {
                            t.album = album_title.clone();
                        }
                        if t.cover_path.is_none() {
                            t.cover_path = album_cover.clone();
                        }
                        if t.album_id.is_none() {
                            t.album_id = album_id_val.clone();
                        }
                        t
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        let data = self
            .api_get(
                "/artist/get",
                &[("artist_id", artist_id), ("extra", "biography")],
            )
            .await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        let data = self
            .api_get("/playlist/get", &[("playlist_id", playlist_id)])
            .await?;
        Ok(StreamPlaylist {
            id: data["id"].as_u64().unwrap_or(0).to_string(),
            name: data["name"].as_str().unwrap_or("").into(),
            description: data["description"].as_str().map(Into::into),
            cover_path: data["image_rectangle_mini"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(Into::into),
            track_count: data["tracks_count"].as_u64().unwrap_or(0) as u32,
            owner: data["owner"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(
                "/playlist/get",
                &[
                    ("playlist_id", playlist_id),
                    ("extra", "tracks"),
                    ("limit", "500"),
                ],
            )
            .await?;
        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_genres(&self, parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        let mut params: Vec<(&str, &str)> = vec![("offset", "0"), ("limit", "500")];
        if let Some(pid) = parent_id {
            params.push(("parent_id", pid));
        }
        let data = self.api_get("/genre/list", &params).await.map_err(|e| {
            info!(error = %e, "qobuz_genres_failed");
            e
        })?;
        let genres: Vec<StreamGenre> = data["genres"]["items"]
            .as_array()
            .or_else(|| data["genres"].as_array())
            .or_else(|| data.as_array())
            .map(|items| items.iter().map(Self::map_genre).collect())
            .unwrap_or_default();
        if genres.is_empty() {
            info!(raw = %data, "qobuz_genres_empty_response");
        }
        Ok(genres)
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let limit_str = limit.to_string();
        let data = self
            .api_get(
                "/album/getFeatured",
                &[
                    ("type", "new-releases"),
                    ("genre_ids", genre_id),
                    ("limit", &limit_str),
                ],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get(
                "/album/getFeatured",
                &[("type", "new-releases"), ("limit", "200")],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        Ok(vec![
            FeaturedSection {
                id: "new-releases".into(),
                name: "New Releases".into(),
            },
            FeaturedSection {
                id: "best-sellers".into(),
                name: "Best Sellers".into(),
            },
            FeaturedSection {
                id: "press-awards".into(),
                name: "Press Awards".into(),
            },
            FeaturedSection {
                id: "editor-picks".into(),
                name: "Editor Picks".into(),
            },
            FeaturedSection {
                id: "most-streamed".into(),
                name: "Most Streamed".into(),
            },
            // Les deux rangées de l'onglet « Le goût de Qobuz », que Tune
            // n'exposait pas alors que l'API les sert depuis toujours (Fabien,
            // comparaison avec Roon).
            FeaturedSection {
                id: "ideal-discography".into(),
                name: "Ideal Discography".into(),
            },
            FeaturedSection {
                id: "qobuzissims".into(),
                name: "Qobuzissimes".into(),
            },
        ])
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get(
                "/album/getFeatured",
                &[("type", section_id), ("limit", "50")],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_album_label(&self, album_id: &str) -> Result<LabelInfo, TuneError> {
        // Resolve the album's label (id + name come straight from album/get).
        let album = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        let label_id = album["label"]["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| album["label"]["id"].as_str().map(Into::into))
            .ok_or_else(|| TuneError::NotFound("album has no label".into()))?;
        let name = album["label"]["name"].as_str().unwrap_or("").into();
        // Bounded pagination: big majors (e.g. Columbia) expose 10k+ albums.
        // We cap at MAX and the app offers a text filter over the loaded set.
        const MAX: usize = 500;
        const PAGE: usize = 50;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut albums: Vec<StreamAlbum> = Vec::new();
        let mut offset: usize = 0;
        loop {
            let off = offset.to_string();
            let lim = PAGE.to_string();
            let data = self
                .api_get(
                    "/label/get",
                    &[
                        ("label_id", &label_id),
                        ("extra", "albums"),
                        ("limit", &lim),
                        ("offset", &off),
                    ],
                )
                .await?;
            let items = data["albums"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let n = items.len();
            // A label's full catalogue includes not-yet-released pre-orders
            // (streamable=true but a future release date) whose tracks resolve
            // to "no url" (502) on playback. Keep only released + streamable
            // albums (mirrors the LMS plugin's `_isReleased` check).
            albums.extend(
                items
                    .iter()
                    .filter(|it| {
                        let streamable = it["streamable"].as_bool() != Some(false);
                        let released = it["released_at"].as_u64().map_or(true, |ts| ts <= now);
                        streamable && released
                    })
                    .map(Self::map_album),
            );
            let total = data["albums"]["total"].as_u64().unwrap_or(0) as usize;
            offset += n;
            if n < PAGE || offset >= total || albums.len() >= MAX {
                break;
            }
        }
        albums.truncate(MAX);
        Ok(LabelInfo {
            id: label_id,
            name,
            albums,
        })
    }

    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        let data = self.api_get("/playlist/getTags", &[]).await?;
        let tags = data["tags"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let id = item["id"]
                            .as_str()
                            .map(Into::into)
                            .or_else(|| item["id"].as_u64().map(|i| i.to_string()))
                            .or_else(|| item["slug"].as_str().map(Into::into))?;
                        // Le libellé est, selon les entrées : une chaîne, un
                        // objet localisé {en, fr, …}, ou — c'est le cas courant
                        // chez Qobuz — un objet localisé ENCODÉ EN JSON dans
                        // `name_json`. Sans cette dernière branche, aucune des
                        // 13 catégories ne trouvait son nom et toutes
                        // retombaient sur leur slug : les rangées s'appelaient
                        // « artist », « mood », « label ».
                        let localized = |v: &serde_json::Value| -> Option<String> {
                            let obj = v.as_object()?;
                            obj.get("fr")
                                .or_else(|| obj.get("en"))
                                .or_else(|| obj.values().next())
                                .and_then(|s| s.as_str())
                                .map(String::from)
                        };
                        let name = item["name"]
                            .as_str()
                            .map(String::from)
                            .or_else(|| localized(&item["name"]))
                            .or_else(|| {
                                item["name_json"]
                                    .as_str()
                                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                                    .as_ref()
                                    .and_then(localized)
                            })
                            .or_else(|| localized(&item["name_json"]))
                            .unwrap_or_else(|| id.clone());
                        Some(PlaylistTag { id, name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tags)
    }

    /// Les playlists éditoriales Qobuz, celles composées par leurs équipes.
    ///
    /// Le client appelle `GET /streaming/{service}/featured`, donc `get_featured`.
    /// Qobuz était le seul service à ne pas la surcharger — Tidal, Deezer et
    /// Spotify le font — et héritait donc de l'implémentation par défaut du
    /// trait, qui renvoie une liste vide. Le carrousel « Playlists en vedette »
    /// ne s'affichait jamais pour Qobuz, alors que tout existait par ailleurs :
    /// la récupération ci-dessous, et même une route dédiée que personne
    /// n'appelait (retour Fabien).
    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.get_featured_playlists(None, None).await
    }

    async fn get_featured_playlists(
        &self,
        tag: Option<&str>,
        genre: Option<&str>,
    ) -> Result<Vec<StreamPlaylist>, TuneError> {
        // Une seule page de 50 laissait la majorité du catalogue éditorial
        // dehors — « il manque beaucoup de playlists éditoriales Qobuz »
        // (Fabien). On pagine, avec un plafond : sans tag, Qobuz en expose
        // plusieurs milliers, et personne ne fait défiler ça.
        const MAX: usize = 500;
        let mut params: Vec<(&str, &str)> = vec![("type", "editor-picks")];
        if let Some(t) = tag {
            params.push(("tags", t));
        }
        if let Some(g) = genre {
            params.push(("genre_ids", g));
        }
        let mut items = self
            .api_get_all_pages("/playlist/getFeatured", &params, "playlists")
            .await?;
        items.truncate(MAX);
        Ok(items.iter().map(Self::map_featured_playlist).collect())
    }

    /// Une rangée par tag Qobuz, dans l'ordre où Qobuz les publie.
    ///
    /// Les tags sont interrogés en parallèle (une requête chacun, une page) :
    /// séquentiellement, une dizaine d'allers-retours mettaient la vue à
    /// plusieurs secondes. Un tag qui échoue ou qui ne renvoie rien est
    /// simplement absent — une rangée vide n'apprend rien à personne.
    async fn get_featured_playlists_by_tag(
        &self,
        genre: Option<&str>,
    ) -> Result<Vec<PlaylistTagGroup>, TuneError> {
        /// Playlists par rangée : au-delà, le carrousel ne sert plus à rien et
        /// la vue s'alourdit. Le détail d'un tag reste accessible par
        /// `get_featured_playlists(tag)`, qui pagine.
        const PER_TAG: usize = 50;

        let tags = self.get_playlist_tags().await?;
        let rows = futures_util::future::join_all(tags.into_iter().map(|tag| async move {
            let limit = PER_TAG.to_string();
            let mut params: Vec<(&str, &str)> = vec![
                ("type", "editor-picks"),
                ("tags", tag.id.as_str()),
                ("limit", &limit),
            ];
            if let Some(g) = genre {
                params.push(("genre_ids", g));
            }
            let data = self.api_get("/playlist/getFeatured", &params).await.ok()?;
            let playlists: Vec<StreamPlaylist> = data["playlists"]["items"]
                .as_array()
                .map(|items| items.iter().map(Self::map_featured_playlist).collect())
                .unwrap_or_default();
            if playlists.is_empty() {
                return None;
            }
            Some(PlaylistTagGroup {
                id: tag.id,
                name: tag.name,
                playlists,
            })
        }))
        .await;
        Ok(rows.into_iter().flatten().collect())
    }

    async fn get_album_context(&self, album_id: &str) -> Result<AlbumContext, TuneError> {
        let album = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        Ok(AlbumContext {
            genre_id: album["genre"]["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| album["genre"]["id"].as_str().map(Into::into)),
            genre_name: album["genre"]["name"].as_str().map(Into::into),
            label_id: album["label"]["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| album["label"]["id"].as_str().map(Into::into)),
            label_name: album["label"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "tracks")],
                "tracks",
            )
            .await?;
        Ok(items.iter().map(Self::map_track).collect())
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let key = favorite_key(fav_type)?;
        self.require_user_token("favorite/create")?;
        let res = self.api_post("/favorite/create", &[(key, item_id)]).await;
        log_favorite_result("create", fav_type, item_id, &res);
        res?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let key = favorite_key(fav_type)?;
        self.require_user_token("favorite/delete")?;
        let res = self.api_post("/favorite/delete", &[(key, item_id)]).await;
        log_favorite_result("delete", fav_type, item_id, &res);
        res?;
        Ok(())
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        let data = self
            .api_get("/playlist/getUserPlaylists", &[("limit", "500")])
            .await?;
        let playlists = data["playlists"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| StreamPlaylist {
                        id: item["id"].as_u64().unwrap_or(0).to_string(),
                        name: item["name"].as_str().unwrap_or("").into(),
                        description: item["description"].as_str().map(Into::into),
                        cover_path: None,
                        track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
                        owner: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get(
                "/artist/get",
                &[
                    ("artist_id", artist_id),
                    ("extra", "albums"),
                    ("limit", "50"),
                ],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(
                "/artist/get",
                &[
                    ("artist_id", artist_id),
                    ("extra", "tracks_appears_on"),
                    ("limit", "20"),
                ],
            )
            .await?;
        let tracks = data["tracks_appears_on"]["items"]
            .as_array()
            .or_else(|| data["tracks"]["items"].as_array())
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<String, TuneError> {
        let desc = description.unwrap_or("Created by Tune");
        let resp = self
            .api_post(
                "/playlist/create",
                &[
                    ("name", name),
                    ("description", desc),
                    ("is_public", "false"),
                ],
            )
            .await?;
        resp["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| resp["id"].as_str().map(|s| s.to_string()))
            .ok_or_else(|| "qobuz: no playlist id in response".into())
    }

    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let mut added = 0;
        for chunk in track_ids.chunks(50) {
            let ids_csv = chunk.join(",");
            self.api_post(
                "/playlist/addTracks",
                &[("playlist_id", playlist_id), ("track_ids", &ids_csv)],
            )
            .await?;
            added += chunk.len();
        }
        Ok(added)
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<(), TuneError> {
        self.api_post("/playlist/delete", &[("playlist_id", playlist_id)])
            .await?;
        Ok(())
    }

    /// Qobuz deletes by `playlist_track_id` (the per-position id), not the
    /// source track id — so resolve them from the playlist first.
    async fn remove_tracks_from_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let data = self
            .api_get(
                "/playlist/get",
                &[
                    ("playlist_id", playlist_id),
                    ("extra", "tracks"),
                    ("limit", "500"),
                ],
            )
            .await?;
        let wanted: std::collections::HashSet<&str> =
            track_ids.iter().map(|s| s.as_str()).collect();
        let mut ptids: Vec<String> = Vec::new();
        if let Some(items) = data["tracks"]["items"].as_array() {
            for item in items {
                let sid = item["id"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                if wanted.contains(sid.as_str()) {
                    if let Some(ptid) = item["playlist_track_id"].as_u64() {
                        ptids.push(ptid.to_string());
                    }
                }
            }
        }
        if ptids.is_empty() {
            return Ok(0);
        }
        let csv = ptids.join(",");
        self.api_post(
            "/playlist/deleteTracks",
            &[("playlist_id", playlist_id), ("playlist_track_ids", &csv)],
        )
        .await?;
        Ok(ptids.len())
    }

    fn supports_write(&self) -> bool {
        self.user_auth_token.is_some()
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "albums")],
                "albums",
            )
            .await?;
        Ok(items.iter().map(Self::map_album).collect())
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "artists")],
                "artists",
            )
            .await?;
        Ok(items.iter().map(Self::map_artist).collect())
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        if self.user_auth_token.is_none() {
            return Ok(false);
        }
        let test = self.api_get("/user/get", &[]).await;
        let Err(ref e) = test else {
            return Ok(false);
        };
        // Only a rejected session is conclusive. A timeout or a 5xx says
        // nothing about the token and must not cost the user their session.
        if !(e.contains("401") || e.contains("403")) {
            return Ok(false);
        }
        if self.auto_relogin().await {
            info!("qobuz_token_refreshed_via_relogin");
            return Ok(true);
        }
        // Qobuz rejected the token and we cannot get a new one. Drop it: while
        // it stayed in place, `auth_status` still answered `authenticated:
        // true`, so the UI showed the service connected, the friendly "session
        // expired, reconnect in Settings" branch in `get_track_url` (which
        // tests for a *missing* token) never ran, and playback failed with a
        // raw `qobuz /track/getFileUrl: 401`. Clearing it makes every one of
        // those report the truth.
        warn!("qobuz_session_expired_token_cleared");
        self.user_auth_token = None;
        self.subscription = None;
        self.session_expired = true;
        // `username` is kept on purpose: the account is still known, it just
        // needs a password again, and the client can name it in the prompt.
        Ok(true)
    }

    fn session_expired(&self) -> bool {
        self.session_expired
    }

    /// The persisted blob deliberately carries **no password**.
    ///
    /// It used to. `settings.auth_tokens_qobuz` held `stored_password` in the
    /// clear next to the token, so anyone who could read `tune.db` — a backup,
    /// a copied WAL, a support bundle — walked away with the account password
    /// itself, not a revocable session. The API responses were already redacted
    /// (`is_secret_key` in `routes/system/config.rs`); the file on disk was not.
    ///
    /// The password now lives in memory only, for the lifetime of the process,
    /// so `auto_relogin` still recovers from an expired token without a round
    /// trip to the user. Across a restart it is gone: if the stored token has
    /// expired by then, the user re-authenticates once. That is the trade —
    /// a rare re-login against a password that is never written down.
    fn save_tokens(&self) -> Option<serde_json::Value> {
        let token = self.user_auth_token.as_ref()?;
        Some(serde_json::json!({
            "user_auth_token": token,
            "username": self.username,
            "subscription": self.subscription,
            "app_id": self.app_id,
            "app_secret": self.app_secret,
            "stored_username": self.stored_username,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(t) = tokens["user_auth_token"].as_str() {
            self.user_auth_token = Some(t.into());
            self.username = tokens["username"].as_str().map(Into::into);
            self.subscription = tokens["subscription"].as_str().map(Into::into);
            if let Some(id) = tokens["app_id"].as_str() {
                self.app_id = id.into();
            }
            if let Some(secret) = tokens["app_secret"].as_str() {
                self.app_secret = secret.into();
            }
            self.stored_username = tokens["stored_username"].as_str().map(Into::into);
            // A row written before this field was dropped still carries the
            // plaintext password. Do not load it into the session — flag the row
            // so the registry rewrites it without the field, which is what
            // actually removes the secret from disk.
            if tokens.get("stored_password").is_some_and(|v| !v.is_null()) {
                warn!("qobuz_legacy_plaintext_password_purged");
                self.needs_token_rewrite = true;
            }
            true
        } else {
            false
        }
    }

    fn tokens_need_rewrite(&self) -> bool {
        self.needs_token_rewrite
    }

    async fn post_restore(&mut self) {
        self.refresh_credentials().await;
        let _ = self.refresh_if_needed().await;
    }
}

fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_order_direct_first_by_default() {
        // All users: direct Qobuz API first, mozaiklabs proxy as fallback.
        assert_eq!(endpoint_order(false), (API_BASE, API_PROXY));
    }

    #[test]
    fn endpoint_order_proxy_first_for_founder() {
        // Founder account (license `qobuz_proxy_first`): proxy first.
        assert_eq!(endpoint_order(true), (API_PROXY, API_BASE));
    }

    #[test]
    fn favorite_key_maps_the_plural_types_sent_by_the_client() {
        assert_eq!(favorite_key("tracks").unwrap(), "track_ids");
        assert_eq!(favorite_key("albums").unwrap(), "album_ids");
        assert_eq!(favorite_key("artists").unwrap(), "artist_ids");
        assert!(
            favorite_key("track").is_err(),
            "le singulier n'est pas le contrat"
        );
    }

    #[test]
    fn writing_a_favorite_without_a_user_token_is_refused_up_front() {
        // Sans jeton utilisateur, Qobuz accepte la requête sans rien
        // enregistrer : on doit échouer avant de l'émettre, pas rapporter un
        // succès que l'app Qobuz dément.
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(svc.user_auth_token.is_none());
        let err = svc.require_user_token("favorite/create").unwrap_err();
        assert!(
            err.to_string().contains("Qobuz"),
            "le message doit désigner le compte à reconnecter, obtenu : {err}"
        );
    }

    #[test]
    fn new_service_defaults_to_direct_first() {
        let svc = QobuzService::new("id".into(), "secret".into());
        assert!(!svc.proxy_first);
    }

    #[test]
    fn attempt_error_transient_classification() {
        // Network errors and 5xx fall back to the other endpoint; 4xx and
        // JSON parse errors are final.
        assert!(AttemptError::Network("timeout".into()).transient());
        assert!(
            AttemptError::Http {
                status: 502,
                body: String::new()
            }
            .transient()
        );
        assert!(
            !AttemptError::Http {
                status: 401,
                body: String::new()
            }
            .transient()
        );
        assert!(!AttemptError::Json("eof".into()).transient());
    }

    #[test]
    fn map_track_basic() {
        let json = json!({
            "id": 12345,
            "title": "Take Five",
            "performer": {"name": "Dave Brubeck"},
            "album": {
                "title": "Time Out",
                "id": 678,
                "image": {"large": "http://img.qobuz.com/large.jpg"},
            },
            "duration": 324,
            "track_number": 2,
            "media_number": 1,
            "parental_warning": false,
            "maximum_sampling_rate": 192.0,
            "maximum_bit_depth": 24,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.id, "12345");
        assert_eq!(track.title, "Take Five");
        assert_eq!(track.artist, "Dave Brubeck");
        assert_eq!(track.album.as_deref(), Some("Time Out"));
        assert_eq!(track.album_id.as_deref(), Some("678"));
        assert_eq!(track.duration_ms, 324_000);
        assert_eq!(track.track_number, Some(2));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(
            track.cover_path.as_deref(),
            Some("http://img.qobuz.com/large.jpg")
        );
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 192000);
        assert_eq!(q.bit_depth, 24);
    }

    #[test]
    fn map_track_artist_fallback() {
        let json = json!({
            "id": 1,
            "title": "Test",
            "artist": {"name": "Fallback Artist"},
            "album": {},
            "duration": 100,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Fallback Artist");
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": 0,
            "title": null,
            "album": {},
            "duration": null,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 44100);
        assert_eq!(q.bit_depth, 16);
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": 999,
            "title": "Time Out",
            "artist": {"name": "Dave Brubeck", "id": 42},
            "image": {"large": "http://img.qobuz.com/album.jpg"},
            "release_date_original": "1959-12-14",
            "tracks_count": 7,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "999");
        assert_eq!(album.title, "Time Out");
        assert_eq!(album.artist, "Dave Brubeck");
        assert_eq!(album.artist_id.as_deref(), Some("42"));
        assert_eq!(album.year, Some(1959));
        assert_eq!(album.track_count, 7);
        assert_eq!(
            album.cover_path.as_deref(),
            Some("http://img.qobuz.com/album.jpg")
        );
    }

    #[test]
    fn map_album_with_released_at_timestamp() {
        let json = json!({
            "id": "abc",
            "title": "Test",
            "artist": {"name": "Test"},
            "released_at": 1580515200, // ~2020
            "tracks_count": 10,
        });
        let album = QobuzService::map_album(&json);
        assert!(album.year.is_some());
        assert!(album.year.unwrap() >= 2019 && album.year.unwrap() <= 2021);
    }

    #[test]
    fn map_album_string_id() {
        let json = json!({
            "id": "abc123",
            "title": "Test",
            "artist": {},
            "tracks_count": 0,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "abc123");
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": 42,
            "name": "Dave Brubeck",
            "image": {"large": "http://img.qobuz.com/artist.jpg"},
        });
        let artist = QobuzService::map_artist(&json);
        assert_eq!(artist.id, "42");
        assert_eq!(artist.name, "Dave Brubeck");
        assert_eq!(
            artist.image_path.as_deref(),
            Some("http://img.qobuz.com/artist.jpg")
        );
    }

    #[test]
    fn map_genre_basic() {
        let json = json!({
            "id": 10,
            "name": "Jazz",
            "subgenres": [{"id": 11, "name": "Bebop"}],
            "image": "http://img.qobuz.com/jazz.jpg",
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/jazz.jpg")
        );
    }

    #[test]
    fn map_genre_subgenres_count() {
        // Qobuz /genre/list returns subgenresCount (integer) instead of subgenres array
        let json = json!({
            "id": 10,
            "name": "Jazz",
            "slug": "jazz",
            "subgenresCount": 15,
            "image": {"large": "http://img.qobuz.com/jazz-large.jpg"},
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/jazz-large.jpg")
        );
    }

    #[test]
    fn map_genre_image_object() {
        // Image as object with large/small keys
        let json = json!({
            "id": 30,
            "name": "Classical",
            "subgenresCount": 5,
            "image": {"large": "http://img.qobuz.com/classical.jpg", "small": "http://img.qobuz.com/classical-sm.jpg"},
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/classical.jpg")
        );
    }

    #[test]
    fn map_genre_no_subgenres() {
        let json = json!({
            "id": 20,
            "name": "Blues",
            "subgenres": [],
        });
        let genre = QobuzService::map_genre(&json);
        assert!(!genre.has_children);
    }

    #[test]
    fn map_genre_slug_heuristic() {
        // Top-level slug (no '/') implies children
        let json = json!({
            "id": 40,
            "name": "Rock",
            "slug": "rock",
        });
        let genre = QobuzService::map_genre(&json);
        assert!(genre.has_children);

        // Sub-genre slug with '/' implies no children
        let json2 = json!({
            "id": 41,
            "name": "Hard Rock",
            "slug": "rock/hard-rock",
        });
        let genre2 = QobuzService::map_genre(&json2);
        assert!(!genre2.has_children);
    }

    #[test]
    fn qobuz_service_name() {
        let svc = QobuzService::new("app123".into(), "secret".into());
        assert_eq!(svc.name(), "qobuz");
    }

    #[test]
    fn qobuz_save_tokens_no_auth() {
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn qobuz_restore_tokens() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "username": "testuser",
            "subscription": "Studio",
            "app_id": "new_app",
            "app_secret": "new_secret",
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.user_auth_token.as_deref(), Some("token123"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
        assert_eq!(svc.app_id, "new_app");
        assert_eq!(svc.app_secret, "new_secret");
    }

    #[test]
    fn qobuz_restore_tokens_invalid() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    #[test]
    fn qobuz_save_tokens_never_persists_the_password() {
        // The whole point of the fix: an authenticated session writes a blob
        // that a reader of tune.db cannot turn into account credentials.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.user_auth_token = Some("token123".into());
        svc.stored_username = Some("testuser".into());
        svc.stored_password = Some("hunter2".into());

        let tokens = svc
            .save_tokens()
            .expect("authenticated, so a blob is written");

        assert!(tokens.get("stored_password").is_none());
        assert_eq!(tokens["stored_username"], "testuser");
        assert_eq!(tokens["user_auth_token"], "token123");
        assert!(
            !tokens.to_string().contains("hunter2"),
            "the password must not reach disk under any key"
        );
    }

    #[test]
    fn qobuz_restore_drops_legacy_password_and_asks_for_a_rewrite() {
        // A row written by a pre-fix build. The password must not be loaded into
        // the session, and the service must ask for the row to be rewritten.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "username": "testuser",
            "stored_username": "testuser",
            "stored_password": "hunter2",
        });

        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.stored_password, None);
        assert!(svc.tokens_need_rewrite());

        // And what gets written back is clean.
        let rewritten = svc.save_tokens().expect("token restored");
        assert!(!rewritten.to_string().contains("hunter2"));
    }

    #[test]
    fn qobuz_restore_clean_row_needs_no_rewrite() {
        // No stale field, no write: startup must not churn the settings row on
        // every boot once the DB has been migrated.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "stored_username": "testuser",
        });

        assert!(svc.restore_tokens(&tokens));
        assert!(!svc.tokens_need_rewrite());
    }

    #[test]
    fn qobuz_auto_relogin_still_works_within_the_session() {
        // The password stays in memory after an interactive login, so an expired
        // token is recovered without prompting. Only a restart forces a re-login.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.stored_username = Some("testuser".into());
        svc.stored_password = Some("hunter2".into());
        assert!(svc.stored_password.is_some());

        // Restoring from disk is the case that cannot: nothing to relogin with.
        let mut restored = QobuzService::new("app".into(), "secret".into());
        restored.restore_tokens(&json!({
            "user_auth_token": "token123",
            "stored_username": "testuser",
        }));
        assert_eq!(restored.stored_password, None);
    }

    #[test]
    fn qobuz_fresh_service_reports_no_expired_session() {
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(!svc.session_expired());
        assert!(!svc.auth_status_internal().authenticated);
    }

    #[test]
    fn qobuz_cleared_session_reports_disconnected() {
        // The state `refresh_if_needed` leaves behind once Qobuz has refused the
        // token: `authenticated` must be false, or the UI keeps showing the
        // service as connected while every call 401s.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.user_auth_token = None;
        svc.subscription = None;
        svc.username = Some("testuser".into());
        svc.session_expired = true;

        let status = svc.auth_status_internal();
        assert!(!status.authenticated);
        assert!(svc.session_expired());
        // The account stays named so the prompt can address it.
        assert_eq!(status.username.as_deref(), Some("testuser"));
        // And nothing is persisted for a session that no longer exists.
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn qobuz_login_clears_the_expired_flag() {
        // Reconnecting must undo the expiry, otherwise the row would be deleted
        // again right after a successful login.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.session_expired = true;

        // What `login_internal` does on success, without the network round trip.
        svc.user_auth_token = Some("fresh".into());
        svc.session_expired = false;

        assert!(!svc.session_expired());
        assert!(svc.auth_status_internal().authenticated);
        assert!(svc.save_tokens().is_some());
    }

    #[test]
    fn qobuz_set_enabled() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.set_enabled(false);
        assert!(!svc.enabled());
        svc.set_enabled(true);
        assert!(svc.enabled());
    }

    #[test]
    fn md5_hex_known_value() {
        // MD5 of empty string is d41d8cd98f00b204e9800998ecf8427e
        let result = md5_hex("");
        assert_eq!(result, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn qobuz_supports_write() {
        let mut svc = QobuzService::new("app_id".into(), "secret".into());
        assert!(!svc.supports_write());
        svc.user_auth_token = Some("token".into());
        assert!(svc.supports_write());
    }
}
