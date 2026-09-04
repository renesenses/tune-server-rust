use super::*;

impl PlaybackOrchestrator {
    /// Like `play`, but does NOT write a listen-history row.  Used for internal
    /// stream re-creations of a track that is *already* being played (seek,
    /// radio auto-retry, reconnect) so a single logical play is not counted
    /// multiple times in the "Historique de lecture".
    pub async fn play_without_history(&self, req: PlayRequest) -> Result<PlayResult, String> {
        self.play_inner(req, false).await
    }

    /// Oublie l'annonce en attente d'une zone navigateur : la lecture s'arrête
    /// sans que l'onglet ait rien tiré, il n'y a donc rien à annoncer.
    pub(super) fn oublier_annonce_navigateur(&self, zone_id: i64) {
        if let Ok(mut en_attente) = self.annonces_navigateur.lock() {
            en_attente.remove(&zone_id);
        }
    }

    /// Dispatch scrobbles to all configured services, respecting tier limits.
    /// Free = 1 service max, Premium = all simultaneously.
    ///
    /// Called by the poller once the current track has been played past the
    /// Last.fm threshold (50% or 4 min), so a scrobble reflects a real listen
    /// rather than a mere play-start (#1113).
    pub fn dispatch_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        // Check tier: if both services are active and user is Free, only
        // dispatch to the first one (Last.fm has priority as legacy default).
        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_scrobble(title, artist, album);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                // Either Last.fm is not active (so LB is the sole service)
                // or user is Premium (simultaneous allowed).
                self.listenbrainz_scrobble(title, artist, album);
            } else {
                debug!(
                    "listenbrainz_scrobble_skipped_free_tier: lastfm active, upgrade to Premium for multi-service"
                );
            }
        }
    }

    /// Dispatch now-playing updates to all configured services, respecting tier limits.
    pub(super) fn dispatch_now_playing(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_now_playing(title, artist, album);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                self.listenbrainz_now_playing(title, artist, album);
            }
        }
    }

    pub(super) fn lastfm_keys(&self) -> Option<(String, String, String)> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let api_key = settings.get("lastfm_api_key").ok().flatten()?;
        let api_secret = settings.get("lastfm_api_secret").ok().flatten()?;
        let session_key = settings.get("lastfm_session_key").ok().flatten()?;
        if api_key.is_empty() || api_secret.is_empty() || session_key.is_empty() {
            return None;
        }
        Some((api_key, api_secret, session_key))
    }

    pub(super) fn lastfm_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        // Send the album too: Last.fm/Pano apps rely on it to fetch the cover
        // (the web site does a looser track-level match), so scrobbles without
        // an album showed no artwork in the apps (#1113).
        let album = album.filter(|a| !a.is_empty()).map(|a| a.to_string());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::scrobble_full(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
                album.as_deref(),
                None,
                timestamp,
            )
            .await
            {
                warn!("lastfm_scrobble_error: {e}");
            }
        });
    }

    pub(super) fn lastfm_now_playing(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        let album = album.filter(|a| !a.is_empty()).map(|a| a.to_string());
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::update_now_playing_full(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
                album.as_deref(),
                None,
            )
            .await
            {
                warn!("lastfm_now_playing_error: {e}");
            }
        });
    }

    pub(super) fn listenbrainz_token(&self) -> Option<String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings
            .get("listenbrainz_token")
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
    }

    pub(super) fn listenbrainz_scrobble(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let payload = serde_json::json!({
                "listen_type": "single",
                "payload": [{
                    "listened_at": timestamp,
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_scrobble_error: {e}");
            }
        });
    }

    pub(super) fn listenbrainz_now_playing(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "listen_type": "playing_now",
                "payload": [{
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_now_playing_error: {e}");
            }
        });
    }
}
