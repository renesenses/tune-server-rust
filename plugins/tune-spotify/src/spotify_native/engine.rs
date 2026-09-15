//! Opt-in, unofficial Spotify source. No Web API application or password input.
//! Pairing uses librespot's own discovery implementation; metadata and audio
//! use the resulting session. This is deliberately not a Connect controller.

// Private engine: only the subprocess dispatcher constructs it.
use super::catalog;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use librespot_core::{Session, authentication::Credentials, config::SessionConfig};
use librespot_discovery::Discovery;
use tokio::sync::oneshot;

use tune_core::TuneError;
use tune_core::streaming::traits::*;

pub const PAIRING_SECONDS: u64 = 180;
const TOKEN_KIND: &str = "librespot-pairing-v1";
const RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

/// The network boundary is replaceable in tests; status and worker dispatch
/// still exercise the real session lifecycle, including invalidation/backoff.
#[async_trait::async_trait]
trait SessionConnector: Send + Sync {
    async fn connect(&self, session: &Session, credentials: Credentials) -> Result<(), TuneError>;
}

struct NetworkConnector;
#[async_trait::async_trait]
impl SessionConnector for NetworkConnector {
    async fn connect(&self, session: &Session, credentials: Credentials) -> Result<(), TuneError> {
        bounded(session.connect(credentials, false)).await
    }
}

/// Shuts down the AP connection even if login is cancelled or times out.
struct Connected(Session);
impl Drop for Connected {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

#[derive(Default)]
struct Account {
    session: Option<Connected>,
    credentials: Option<Credentials>,
    pairing_until: Option<std::time::Instant>,
    error: Option<String>,
    dirty: bool,
    generation: u64,
    retry_after: Option<tokio::time::Instant>,
}

pub struct SpotifyNativeService {
    enabled: bool,
    device_id: String,
    account: Arc<Mutex<Account>>,
    pairing_cancel: Option<oneshot::Sender<()>>,
    reconnect: tokio::sync::Mutex<()>,
    connector: Arc<dyn SessionConnector>,
}

impl Default for SpotifyNativeService {
    fn default() -> Self {
        Self::new()
    }
}

impl SpotifyNativeService {
    pub fn new() -> Self {
        Self {
            enabled: true,
            device_id: uuid::Uuid::new_v4().to_string(),
            account: Arc::new(Mutex::new(Account::default())),
            pairing_cancel: None,
            reconnect: tokio::sync::Mutex::new(()),
            connector: Arc::new(NetworkConnector),
        }
    }

    fn config(&self) -> SessionConfig {
        SessionConfig {
            device_id: self.device_id.clone(),
            autoplay: Some(false),
            ..SessionConfig::default()
        }
    }

    fn cancel_pairing(&mut self) {
        {
            let mut account = self.account.lock().unwrap();
            account.generation = account.generation.wrapping_add(1);
            account.pairing_until = None;
        }
        if let Some(cancel) = self.pairing_cancel.take() {
            let _ = cancel.send(());
        }
    }

    fn disconnect(&mut self) {
        self.cancel_pairing();
        self.account.lock().unwrap().session.take();
    }

    pub(super) async fn session(&self) -> Result<Session, TuneError> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        let _connecting = self.reconnect.lock().await;
        let credentials = {
            let account = self.account.lock().unwrap();
            if let Some(session) = &account.session {
                if !session.0.is_invalid() {
                    return Ok(session.0.clone());
                }
            }
            account.credentials.clone().ok_or_else(|| {
                TuneError::from("Spotify: pair Tune from the Spotify app first".to_owned())
            })?
        };
        let session = Connected(Session::new(self.config(), None));
        if let Err(error) = self.connector.connect(&session.0, credentials).await {
            let mut account = self.account.lock().unwrap();
            account.retry_after = Some(tokio::time::Instant::now() + RECONNECT_BACKOFF);
            account.error = Some("Spotify reconnect failed; retrying with saved pairing".into());
            return Err(error);
        }
        let result = session.0.clone();
        let mut account = self.account.lock().unwrap();
        account.session = Some(session);
        account.retry_after = None;
        account.error = None;
        Ok(result)
    }

    /// A status request may renew an existing pairing, but must never create
    /// one. Snapshot reads remain free of I/O (including the worker's reply).
    pub(super) async fn poll_status(&self) -> AuthStatus {
        let reconnect = {
            let account = self.account.lock().unwrap();
            self.enabled
                && account.credentials.is_some()
                && account.pairing_until.is_none()
                && account.session.as_ref().is_none_or(|s| s.0.is_invalid())
                && account
                    .retry_after
                    .is_none_or(|at| tokio::time::Instant::now() >= at)
        };
        if reconnect {
            let _ = self.session().await;
        }
        self.auth_status().await
    }

    /// Safe diagnostic payload: never returns credentials or access tokens.
    pub fn pairing_status(&self) -> serde_json::Value {
        let account = self.account.lock().unwrap();
        serde_json::json!({
            "mode": "unofficial-native",
            "device_name": "Tune — Spotify pairing",
            "instructions": "On the same local network, open Spotify and select Tune — Spotify pairing in Available devices. Then return to Tune.",
            "pairing": account.pairing_until.is_some_and(|until| until > std::time::Instant::now()),
            "error": account.error,
        })
    }
}

impl Drop for SpotifyNativeService {
    fn drop(&mut self) {
        self.disconnect();
    }
}

async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, librespot_core::Error>>,
) -> Result<T, TuneError> {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .map_err(|_| TuneError::from("Spotify session request timed out".to_owned()))?
        .map_err(|error| TuneError::from(format!("Spotify session: {error}")))
}

fn unsupported(operation: &str) -> TuneError {
    TuneError::Unsupported(format!(
        "Spotify native prototype: {operation} not implemented"
    ))
}

#[async_trait::async_trait]
impl StreamingService for SpotifyNativeService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "spotify"
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.disconnect();
        }
        self.enabled = enabled;
    }

    async fn authenticate(&mut self, input: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        // Status polling must NEVER reopen discovery or extend its deadline.
        if input == &serde_json::json!({"poll": true}) {
            return Ok(self.poll_status().await);
        }
        // Never turn this route into a password/token importer. Re-pairing an
        // account requires logout first, so a stray LAN client cannot replace it.
        if input != &serde_json::json!({"device_flow": true})
            && input.as_object().is_none_or(|o| !o.is_empty())
        {
            return Err("Spotify native uses app pairing; send an empty object".into());
        }
        if self.account.lock().unwrap().credentials.is_some() {
            self.session().await?;
            return Ok(self.auth_status().await);
        }
        if self
            .account
            .lock()
            .unwrap()
            .pairing_until
            .is_some_and(|until| until > std::time::Instant::now())
        {
            return Ok(self.auth_status().await);
        }
        self.cancel_pairing();
        let config = self.config();
        let mut discovery = Discovery::builder(config.device_id.clone(), config.client_id.clone())
            .name("Tune — Spotify pairing")
            .device_type(librespot_discovery::DeviceType::Speaker)
            .launch()
            .map_err(|_| {
                TuneError::from("Spotify pairing could not start on the local network".to_owned())
            })?;
        let (cancel, mut cancelled) = oneshot::channel();
        self.pairing_cancel = Some(cancel);
        let account = self.account.clone();
        let generation = {
            let mut state = account.lock().unwrap();
            state.error = None;
            state.pairing_until =
                Some(std::time::Instant::now() + Duration::from_secs(PAIRING_SECONDS));
            state.generation
        };
        tokio::spawn(async move {
            let credentials = tokio::select! {
                biased;
                _ = &mut cancelled => None,
                result = tokio::time::timeout(Duration::from_secs(PAIRING_SECONDS), discovery.next()) => result.ok().flatten(),
            };
            let _ = tokio::time::timeout(Duration::from_secs(3), discovery.shutdown()).await;
            if let Some(credentials) = credentials {
                let session = Connected(Session::new(config, None));
                let login = tokio::select! {
                    biased;
                    _ = &mut cancelled => None,
                    result = bounded(session.0.connect(credentials.clone(), false)) => Some(result),
                };
                let mut state = account.lock().unwrap();
                // Logout/disable clears pairing_until before signalling cancellation.
                if state.generation == generation && state.pairing_until.is_some() {
                    if matches!(login, Some(Ok(()))) {
                        state.credentials = Some(credentials);
                        state.session = Some(session);
                        state.dirty = true;
                    } else if login.is_some() {
                        state.error =
                            Some("Spotify refused pairing; check Premium and retry".into());
                    }
                }
            } else {
                let mut state = account.lock().unwrap();
                if state.generation == generation && state.pairing_until.is_some() {
                    state.error = Some("Spotify pairing expired or discovery stopped".into());
                }
            }
            let mut state = account.lock().unwrap();
            if state.generation == generation {
                state.pairing_until = None;
            }
        });
        Ok(self.auth_status().await)
    }

    async fn auth_status(&self) -> AuthStatus {
        let state = self.account.lock().unwrap();
        let session = state.session.as_ref().filter(|s| !s.0.is_invalid());
        AuthStatus {
            authenticated: self.enabled && session.is_some(),
            verification_url: Some("/api/v1/streaming/spotify/native-pairing".into()),
            username: session.map(|s| s.0.username()),
            // Only report what the session actually said, not an assumed plan.
            subscription: session.and_then(|s| s.0.get_user_attribute("type")),
            expires_in: state.pairing_until.map(|until| {
                until
                    .saturating_duration_since(std::time::Instant::now())
                    .as_secs()
            }),
            ..Default::default()
        }
    }

    fn auth_details(&self) -> Option<serde_json::Value> {
        Some(self.pairing_status())
    }
    fn credential_key(&self) -> String {
        "auth_tokens_spotify_native".into()
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.disconnect();
        let mut state = self.account.lock().unwrap();
        let generation = state.generation;
        *state = Account {
            generation,
            ..Default::default()
        };
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        catalog::search(&self.session().await?, query, limit).await
    }
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        if offset > 0 {
            return Ok(SearchPage::au_dela(offset));
        }
        let bound = if limit == 0 { 30 } else { limit.min(30) };
        Ok(SearchPage::page_unique_bornee(
            self.search(query, bound).await?,
            bound,
        ))
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        catalog::track(&self.session().await?, id).await
    }
    async fn get_track_url(&self, _: &str, _: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(unsupported("public audio URL; play through a Tune zone"))
    }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, TuneError> {
        catalog::album(&self.session().await?, id).await
    }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        catalog::album_tracks(&self.session().await?, id).await
    }
    async fn get_artist(&self, id: &str) -> Result<StreamArtist, TuneError> {
        catalog::artist(&self.session().await?, id).await
    }
    async fn get_artist_albums(&self, id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        catalog::artist_albums(&self.session().await?, id).await
    }
    async fn get_playlist(&self, id: &str) -> Result<StreamPlaylist, TuneError> {
        catalog::playlist(&self.session().await?, id).await
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        catalog::playlist_tracks(&self.session().await?, id).await
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.get_playlist_library().await?.into_complete()
    }
    async fn get_playlist_library(&self) -> Result<PlaylistLibrary, TuneError> {
        super::library::user_playlists(&self.session().await?).await
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        let session = self.session().await?;
        let uris = super::liked::uris(&session).await?;
        super::metadata::tracks(&session, uris).await
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let session = self.session().await?;
        let uris = super::saved::album_uris(&session).await?;
        super::metadata::albums(&session, uris).await
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        let session = self.session().await?;
        let uris = super::following::artist_uris(&session).await?;
        super::metadata::artists(&session, uris).await
    }

    fn save_tokens(&self) -> Option<serde_json::Value> {
        // A tombstone after logout prevents reloading an older credential.
        Some(serde_json::json!({
            "kind": TOKEN_KIND, "device_id": self.device_id,
            "credentials": self.account.lock().unwrap().credentials,
        }))
    }
    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if tokens["kind"] != TOKEN_KIND {
            return false;
        }
        let Ok(credentials) = serde_json::from_value::<Credentials>(tokens["credentials"].clone())
        else {
            return false;
        };
        if credentials.auth_data.is_empty() {
            return false;
        }
        let Some(device_id) = tokens["device_id"]
            .as_str()
            .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        else {
            return false;
        };
        self.device_id = device_id.to_owned();
        self.account.lock().unwrap().credentials = Some(credentials);
        true
    }
    async fn post_restore(&mut self) {
        self.poll_status().await;
    }
    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        let mut state = self.account.lock().unwrap();
        Ok(std::mem::take(&mut state.dirty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FixtureConnector {
        attempts: AtomicUsize,
        fail: AtomicBool,
    }
    #[async_trait::async_trait]
    impl SessionConnector for FixtureConnector {
        async fn connect(&self, _: &Session, _: Credentials) -> Result<(), TuneError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("fixture: network unavailable; private diagnostic".into())
            } else {
                Ok(())
            }
        }
    }

    fn paired_fixture() -> (SpotifyNativeService, Arc<FixtureConnector>) {
        super::super::worker::initialize_tls();
        let mut service = SpotifyNativeService::new();
        let connector = Arc::new(FixtureConnector::default());
        service.connector = connector.clone();
        service.account.lock().unwrap().credentials =
            Some(Credentials::with_access_token("fixture-only"));
        (service, connector)
    }

    async fn worker_status(service: &mut SpotifyNativeService) -> AuthStatus {
        serde_json::from_value(
            super::super::worker::execute(service, super::super::ipc::Operation::Status)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn native_worker_status_reconnects_invalid_session_without_pairing() {
        let (mut service, connector) = paired_fixture();
        service.session().await.unwrap().shutdown();
        assert!(!service.auth_status().await.authenticated);
        let credentials = service.save_tokens().unwrap();
        assert!(
            worker_status(&mut service).await.authenticated,
            "Spotify status must reconnect an invalid saved session without a search"
        );
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
        assert!(worker_status(&mut service).await.authenticated);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
        assert!(!service.pairing_status()["pairing"].as_bool().unwrap());
        assert_eq!(service.save_tokens().unwrap(), credentials);
    }

    #[tokio::test(start_paused = true)]
    async fn native_status_reconnect_failure_keeps_pairing_and_backs_off() {
        let (mut service, connector) = paired_fixture();
        connector.fail.store(true, Ordering::SeqCst);
        let credentials = service.save_tokens().unwrap();
        assert!(!worker_status(&mut service).await.authenticated);
        for _ in 0..3 {
            assert!(!worker_status(&mut service).await.authenticated);
        }
        assert_eq!(
            connector.attempts.load(Ordering::SeqCst),
            1,
            "Spotify status must back off after a failed reconnect"
        );
        assert_eq!(service.save_tokens().unwrap(), credentials);
        assert!(
            !service
                .pairing_status()
                .to_string()
                .contains("private diagnostic")
        );
        assert!(!service.pairing_status()["pairing"].as_bool().unwrap());
        tokio::time::advance(RECONNECT_BACKOFF).await;
        connector.fail.store(false, Ordering::SeqCst);
        assert!(worker_status(&mut service).await.authenticated);
        assert_eq!(connector.attempts.load(Ordering::SeqCst), 2);
        assert!(service.pairing_status()["error"].is_null());
    }

    #[tokio::test]
    async fn native_status_cannot_reconnect_disabled_logged_out_or_pairing_accounts() {
        let (mut service, connector) = paired_fixture();
        service.set_enabled(false);
        assert!(!worker_status(&mut service).await.authenticated);
        service.set_enabled(true);
        service.account.lock().unwrap().pairing_until =
            Some(std::time::Instant::now() + Duration::from_secs(60));
        assert!(!worker_status(&mut service).await.authenticated);
        service.logout().await.unwrap();
        assert!(!worker_status(&mut service).await.authenticated);
        assert_eq!(
            connector.attempts.load(Ordering::SeqCst),
            0,
            "A status read must not reconnect after logout/disable or interfere with pairing"
        );
        assert!(!service.pairing_status()["pairing"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn native_starts_disconnected_and_refuses_password_input() {
        let mut service = SpotifyNativeService::new();
        assert!(!service.auth_status().await.authenticated);
        assert!(
            service
                .authenticate(&serde_json::json!({"password":"never-send-me"}))
                .await
                .is_err()
        );
        assert!(!service.pairing_status()["pairing"].as_bool().unwrap());
    }
    #[tokio::test]
    async fn native_logout_persists_tombstone_and_does_not_import_web_tokens() {
        let mut service = SpotifyNativeService::new();
        assert!(!service.restore_tokens(&serde_json::json!({"access_token":"old-web-token"})));
        let credentials = Credentials::with_access_token("fixture-only");
        service.account.lock().unwrap().credentials = Some(credentials);
        assert!(service.save_tokens().unwrap()["credentials"].is_object());
        service.logout().await.unwrap();
        assert!(
            service.save_tokens().unwrap()["credentials"].is_null(),
            "logout must erase persisted Spotify credentials"
        );
        assert!(!service.restore_tokens(&service.save_tokens().unwrap()));
    }
    #[tokio::test]
    async fn native_disabled_cannot_pair_or_read_catalogue() {
        let mut service = SpotifyNativeService::new();
        service.set_enabled(false);
        assert!(service.authenticate(&serde_json::json!({})).await.is_err());
        assert!(service.get_track("4uLU6hMCjMI75M1A2tKUQC").await.is_err());
    }
    #[tokio::test]
    async fn native_status_poll_never_opens_pairing_and_credentials_are_isolated() {
        let mut service = SpotifyNativeService::new();
        assert!(
            !service
                .authenticate(&serde_json::json!({"poll": true}))
                .await
                .unwrap()
                .authenticated
        );
        assert!(!service.pairing_status()["pairing"].as_bool().unwrap());
        assert_eq!(service.credential_key(), "auth_tokens_spotify_native");
        assert_ne!(
            service.credential_key(),
            crate::spotify::SpotifyService::new().credential_key()
        );
    }

    #[tokio::test]
    async fn native_registry_save_and_logout_preserve_existing_web_credentials() {
        use tune_core::db::{backend::DbBackend, settings_repo::SettingsRepo, sqlite::SqliteDb};
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(db.clone());
        settings
            .set("auth_tokens_spotify", "legacy-web-fixture")
            .unwrap();
        // Exercise the public parent-side proxy registered by Tune, not only
        // the private engine now hosted in a subprocess.
        let mut service = super::super::SpotifyNativeService::new();
        assert!(service.restore_tokens(&serde_json::json!({
            "kind": TOKEN_KIND,
            "device_id": uuid::Uuid::new_v4().to_string(),
            "credentials": Credentials::with_access_token("native-fixture"),
        })));
        let mut registry = tune_core::streaming::ServiceRegistry::new();
        registry.register(Box::new(service));
        registry.save_all_tokens(&db).await;
        assert_eq!(
            settings.get("auth_tokens_spotify").unwrap().as_deref(),
            Some("legacy-web-fixture"),
            "native pairing must not overwrite Web API credentials"
        );
        assert!(
            settings
                .get("auth_tokens_spotify_native")
                .unwrap()
                .unwrap()
                .contains("credentials")
        );
        registry
            .get("spotify")
            .unwrap()
            .write()
            .await
            .logout()
            .await
            .unwrap();
        registry.save_all_tokens(&db).await;
        assert_eq!(
            settings.get("auth_tokens_spotify").unwrap().as_deref(),
            Some("legacy-web-fixture")
        );
        let native: serde_json::Value =
            serde_json::from_str(&settings.get("auth_tokens_spotify_native").unwrap().unwrap())
                .unwrap();
        assert!(
            native["credentials"].is_null(),
            "logout must remove native credentials from storage"
        );
    }
}
