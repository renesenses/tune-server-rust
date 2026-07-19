use std::time::Duration;

use reqwest::Client;
use tracing::{debug, info};

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
    use_proxy: bool,
    stored_username: Option<String>,
    stored_password: Option<String>,
    enabled_override: Option<bool>,
}

impl QobuzService {
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
            use_proxy: true,
            stored_username: None,
            stored_password: None,
            enabled_override: None,
        }
    }

    fn api_base(&self) -> &str {
        if self.use_proxy { API_PROXY } else { API_BASE }
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

    async fn api_get(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let base = self.api_base();
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

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if self.use_proxy => {
                // Proxy unreachable (timeout/network) — fallback to direct API
                info!(path, error = %e, "qobuz_proxy_failed_trying_direct");
                return self.api_get_direct(path, params).await;
            }
            Err(e) => return Err(format!("qobuz api: {e}")),
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            // If proxy returned 5xx, try direct
            if self.use_proxy && status >= 500 {
                info!(path, status, "qobuz_proxy_5xx_trying_direct");
                return self.api_get_direct(path, params).await;
            }
            info!(path, status, body = %body, "qobuz_api_error");
            return Err(format!("qobuz {path}: {status} {body}"));
        }

        resp.json().await.map_err(|e| format!("qobuz json: {e}"))
    }

    /// Direct API call bypassing the proxy. Used as fallback when proxy is down.
    async fn api_get_direct(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let url = format!("{API_BASE}{path}");
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

        let resp = req.send().await.map_err(|e| format!("qobuz direct: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            info!(path, status, body = %body, "qobuz_direct_api_error");
            return Err(format!("qobuz {path}: {status} {body}"));
        }

        resp.json().await.map_err(|e| format!("qobuz json: {e}"))
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

    async fn login_internal(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<AuthStatus, String> {
        self.refresh_credentials().await;

        let base = self.api_base();
        let resp = self
            .client
            .post(format!("{base}/user/login"))
            .query(&[("app_id", self.app_id.as_str())])
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|e| format!("qobuz login: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            info!(status, body = %body, "qobuz_login_failed");
            return Err(format!("qobuz login {status}: {body}"));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
        self.user_auth_token = data["user_auth_token"].as_str().map(Into::into);
        self.username = data["user"]["display_name"].as_str().map(Into::into);
        self.subscription = data["user"]["credential"]["label"].as_str().map(Into::into);

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
        if let (Some(u), Some(p)) = (self.stored_username.clone(), self.stored_password.clone()) {
            info!("qobuz_auto_relogin");
            self.login_internal(&u, &p).await.is_ok()
        } else {
            false
        }
    }

    async fn api_post(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let base = self.api_base();
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

        let resp = req.send().await.map_err(|e| format!("qobuz post: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("qobuz {path}: {status} {body}"));
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
                        // name is a localized object {en, fr, ...} or a plain string.
                        let name = item["name"]
                            .as_str()
                            .map(String::from)
                            .or_else(|| item["name"]["en"].as_str().map(String::from))
                            .or_else(|| {
                                item["name"]
                                    .as_object()
                                    .and_then(|m| m.values().next())
                                    .and_then(|v| v.as_str())
                                    .map(String::from)
                            })
                            .unwrap_or_else(|| id.clone());
                        Some(PlaylistTag { id, name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tags)
    }

    async fn get_featured_playlists(
        &self,
        tag: Option<&str>,
        genre: Option<&str>,
    ) -> Result<Vec<StreamPlaylist>, TuneError> {
        let mut params: Vec<(&str, &str)> = vec![("type", "editor-picks"), ("limit", "50")];
        if let Some(t) = tag {
            params.push(("tags", t));
        }
        if let Some(g) = genre {
            params.push(("genre_ids", g));
        }
        let data = self.api_get("/playlist/getFeatured", &params).await?;
        let playlists = data["playlists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_featured_playlist).collect())
            .unwrap_or_default();
        Ok(playlists)
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
        let key = match fav_type {
            "tracks" => "track_ids",
            "albums" => "album_ids",
            "artists" => "artist_ids",
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_post("/favorite/create", &[(key, item_id)]).await?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let key = match fav_type {
            "tracks" => "track_ids",
            "albums" => "album_ids",
            "artists" => "artist_ids",
            _ => return Err(format!("unknown favorite type: {fav_type}").into()),
        };
        self.api_post("/favorite/delete", &[(key, item_id)]).await?;
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
        if let Err(ref e) = test
            && (e.contains("401") || e.contains("403"))
            && self.auto_relogin().await
        {
            info!("qobuz_token_refreshed_via_relogin");
            return Ok(true);
        }
        Ok(false)
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        let token = self.user_auth_token.as_ref()?;
        Some(serde_json::json!({
            "user_auth_token": token,
            "username": self.username,
            "subscription": self.subscription,
            "app_id": self.app_id,
            "app_secret": self.app_secret,
            "stored_username": self.stored_username,
            "stored_password": self.stored_password,
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
            self.stored_password = tokens["stored_password"].as_str().map(Into::into);
            true
        } else {
            false
        }
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
