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

    /// Le palier **effectif** pour les annonces d'ecoute (#3673).
    ///
    /// Ce qui vivait ici lisait la LIGNE `license_tier` de la table `settings`,
    /// recopiee dans les deux fonctions ci-dessous. Cette ligne n'est PAS le
    /// palier effectif, et elle s'en ecarte dans les deux sens :
    ///
    /// - un abonne **par compte** (SSO) n'a pas de cle : `set_account_premium`
    ///   (`license.rs`) ecrit `mozaik_premium`, **jamais** `license_tier`. La
    ///   ligne y vaut donc « free » alors que
    ///   `POST /scrobbler/connect/listenbrainz` a ACCEPTE la seconde connexion
    ///   sur `is_premium()` et que `GET /scrobbler/status` affiche premium.
    ///   ListenBrainz etait ensuite saute a chaque piste, sous un `debug!`
    ///   que personne ne lit ;
    /// - une cle dont la **grace** est ecoulee reste « premium » EN BASE : la
    ///   degradation de `LicenseManager::new_with_limit` ne touche que la
    ///   memoire. Le multi-scrobble continuait donc apres l'expiration.
    ///
    /// Une licence absente (tests, `tune-cli`) ne vaut pas une autorisation :
    /// c'est `false`, le comportement qu'avait deja la ligne absente en base.
    /// Le plafond de zones fait le choix inverse (`enforce_zone_cap` laisse
    /// passer sans licence) parce qu'il REFUSE un geste ; ici on ACCORDE un
    /// droit, et les deux defauts se lisent dans le sens de la prudence.
    pub(super) async fn premium_pour_annonces(&self) -> bool {
        match self.license {
            Some(ref lic) => lic.is_premium().await,
            None => false,
        }
    }

    /// Dispatch scrobbles to all configured services, respecting tier limits.
    /// Free = 1 service max, Premium = all simultaneously.
    ///
    /// Called by the poller once the current track has been played past the
    /// Last.fm threshold (50% or 4 min), so a scrobble reflects a real listen
    /// rather than a mere play-start (#1113).
    pub async fn dispatch_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        // Check tier: if both services are active and user is Free, only
        // dispatch to the first one (Last.fm has priority as legacy default).
        let is_premium = self.premium_pour_annonces().await;

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
    pub(super) async fn dispatch_now_playing(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) {
        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        let is_premium = self.premium_pour_annonces().await;

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

// ---------------------------------------------------------------------------
// #3673 — le palier lu pour les annonces d'ecoute
// ---------------------------------------------------------------------------
#[cfg(test)]
mod palier_des_annonces {
    use super::*;

    fn base_vierge() -> Arc<dyn crate::db::backend::DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn orchestrateur(db: Arc<dyn crate::db::backend::DbBackend>) -> PlaybackOrchestrator {
        let mut orch = PlaybackOrchestrator::new(
            db.clone(),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        );
        // Exactement le cablage de production (`tune-server/src/state.rs`).
        orch.license = Some(Arc::new(crate::license::LicenseManager::new(db)));
        orch
    }

    fn maintenant() -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn il_y_a_jours(n: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(n))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    /// Premier sens : un abonne **par compte** (SSO) doit garder le
    /// multi-scrobble.
    ///
    /// `set_account_premium` n'ecrit jamais `license_tier` : la ligne reste
    /// absente, et la lecture brute de cette ligne rendait « free » pour
    /// quelqu'un que `POST /scrobbler/connect/listenbrainz` venait d'AUTORISER
    /// a brancher un second service. ListenBrainz etait alors saute a chaque
    /// piste, sous un `debug!`.
    #[tokio::test]
    async fn un_abonne_par_compte_garde_le_multi_scrobble() {
        let db = base_vierge();
        let reglages = SettingsRepo::with_backend(db.clone());
        reglages.set("mozaik_premium", "true").unwrap();
        reglages
            .set("mozaik_premium_checked", &maintenant())
            .unwrap();
        assert_eq!(
            reglages.get("license_tier").unwrap(),
            None,
            "le montage du temoin doit reproduire le cas reel : aucune ligne \
             `license_tier`, puisque seul un compte SSO accorde le premium ici"
        );

        let orch = orchestrateur(db);
        assert!(
            orch.license.as_ref().unwrap().is_premium().await,
            "contre-controle du montage : le palier EFFECTIF doit bien etre premium"
        );
        assert!(
            orch.premium_pour_annonces().await,
            "un abonne par COMPTE perd le multi-scrobble : les annonces lisent \
             encore la ligne `license_tier`, que `set_account_premium` n'ecrit \
             jamais (#3673)"
        );
    }

    /// Second sens, le dangereux : une cle dont la **grace** est ecoulee ne
    /// doit plus rien accorder.
    ///
    /// La degradation de `LicenseManager::new_with_limit` ne touche que la
    /// MEMOIRE : la ligne `license_tier` reste « premium » en base a jamais.
    /// Une lecture brute de cette ligne prolongeait donc le multi-scrobble
    /// bien au-dela de l'expiration.
    #[tokio::test]
    async fn une_cle_hors_grace_ne_garde_pas_le_multi_scrobble() {
        let db = base_vierge();
        let reglages = SettingsRepo::with_backend(db.clone());
        reglages.set("license_tier", "premium").unwrap();
        reglages
            .set("license_last_validated", &il_y_a_jours(40))
            .unwrap();

        let orch = orchestrateur(db.clone());
        assert_eq!(
            SettingsRepo::with_backend(db)
                .get("license_tier")
                .unwrap()
                .as_deref(),
            Some("premium"),
            "contre-controle du montage : la LIGNE reste « premium » apres la \
             degradation, qui n'a lieu qu'en memoire"
        );
        assert!(
            !orch.license.as_ref().unwrap().is_premium().await,
            "contre-controle du montage : le palier EFFECTIF doit etre retombe en Free"
        );
        assert!(
            !orch.premium_pour_annonces().await,
            "une cle hors grace garde le multi-scrobble : les annonces lisent \
             encore la ligne `license_tier`, que rien ne remet a « free » (#3673)"
        );
    }

    /// Licence absente (tests, `tune-cli`) : pas de droit. Une licence qu'on
    /// n'a pas ne vaut pas une autorisation — et c'est deja ce que faisait la
    /// ligne absente en base, donc aucun comportement ne change ici.
    #[tokio::test]
    async fn sans_licence_les_annonces_ne_sont_pas_premium() {
        let db = base_vierge();
        let orch = PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        );
        assert!(orch.license.is_none(), "montage : aucune licence branchee");
        assert!(
            !orch.premium_pour_annonces().await,
            "une licence absente vaut autorisation : un hote qui oublie de \
             brancher la licence ouvrirait le multi-scrobble a tous"
        );
    }
}
