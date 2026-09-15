//! Unofficial Spotify source. All librespot sessions/decoders run in children.
//! The server owns credentials, the queue and bounded PCM HTTP sessions.
mod audio;
mod catalog;
mod collections;
mod engine;
mod ipc;
mod library;
mod liked;
mod metadata;
mod worker;

use crate::TuneError;
use crate::streaming::traits::*;
use ipc::{ChildProcess, Operation, Reply};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const PAIRING_SECONDS: u64 = 180;
const TOKEN_KIND: &str = "librespot-pairing-v1";
pub use worker::run_worker_if_requested;

struct Snapshot {
    status: AuthStatus,
    details: Value,
    tokens: Value,
    dirty: bool,
}

struct WorkerClient {
    process: tokio::sync::Mutex<Option<ChildProcess>>,
    life: Mutex<Option<(Arc<AtomicBool>, tokio::sync::watch::Sender<bool>)>>,
    snapshot: Mutex<Snapshot>,
}

impl WorkerClient {
    fn new() -> Self {
        Self {
            process: tokio::sync::Mutex::new(None),
            life: Mutex::new(None),
            snapshot: Mutex::new(Snapshot {
                status: AuthStatus::default(),
                details: json!({"pairing": false, "error": null}),
                tokens: json!({"kind": TOKEN_KIND, "device_id": uuid::Uuid::new_v4().to_string(), "credentials": null}),
                dirty: false,
            }),
        }
    }
    fn alive(&self) -> bool {
        self.life
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(alive, _)| alive.load(Ordering::Acquire))
    }
    fn stop(&self) {
        if let Some((alive, kill)) = self.life.lock().unwrap().take() {
            alive.store(false, Ordering::Release);
            let _ = kill.send(true);
        }
    }
    fn tokens(&self) -> Value {
        self.snapshot.lock().unwrap().tokens.clone()
    }
    fn record(&self, reply: &Reply) {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot.tokens != reply.tokens {
            snapshot.dirty = true;
            snapshot.tokens = reply.tokens.clone();
        }
        snapshot.status = reply.status.clone();
        snapshot.details = reply.details.clone();
    }
    async fn request(&self, operation: Operation) -> Result<Value, TuneError> {
        let mut slot = self.process.lock().await;
        // Keep the process local until its COMPLETE reply was read. Dropping
        // this future kills it, so an orphan response cannot satisfy the next RPC.
        let mut process = match slot.take().filter(|process| process.alive()) {
            Some(process) => process,
            None => {
                let mut process = ChildProcess::spawn("control")?;
                *self.life.lock().unwrap() = Some((process.life.clone(), process.kill.clone()));
                let reply = process
                    .rpc(&Operation::Init {
                        tokens: self.tokens(),
                    })
                    .await?;
                self.record(&reply);
                reply.result.map_err(ipc::Failure::into_tune)?;
                process
            }
        };
        let reply = process.rpc(&operation).await?;
        self.record(&reply);
        *slot = Some(process);
        reply.result.map_err(ipc::Failure::into_tune)
    }
}
impl Drop for WorkerClient {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct SpotifyNativeService {
    enabled: bool,
    client: Arc<WorkerClient>,
    audio: Arc<Mutex<Option<audio::Lease>>>,
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
            client: Arc::new(WorkerClient::new()),
            audio: Arc::new(Mutex::new(None)),
        }
    }
    async fn call<T: DeserializeOwned>(&self, operation: Operation) -> Result<T, TuneError> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        if !self.has_credentials() && !matches!(operation, Operation::Pair | Operation::Status) {
            return Err("Spotify: pair Tune from the Spotify app first".into());
        }
        serde_json::from_value(self.client.request(operation).await?)
            .map_err(|_| "Spotify worker returned an invalid response".into())
    }
    fn has_credentials(&self) -> bool {
        self.client.tokens()["credentials"].is_object()
    }
    pub fn pairing_status(&self) -> Value {
        let alive = self.client.alive();
        let snapshot = self.client.snapshot.lock().unwrap();
        json!({
            "mode": "unofficial-native", "process_isolated": true,
            "device_name": "Tune — Spotify pairing",
            "instructions": "On the same local network, open Spotify and select Tune — Spotify pairing in Available devices. Then return to Tune.",
            "pairing": self.enabled && alive && snapshot.details["pairing"] == true,
            "error": if alive { snapshot.details["error"].clone() } else if snapshot.status.authenticated {
                json!("Spotify worker stopped; retry to reconnect")
            } else { Value::Null },
        })
    }
}
impl Drop for SpotifyNativeService {
    fn drop(&mut self) {
        self.audio.lock().unwrap().take();
        self.client.stop();
    }
}
async fn bounded<T>(
    future: impl std::future::Future<Output = Result<T, librespot_core::Error>>,
) -> Result<T, TuneError> {
    tokio::time::timeout(std::time::Duration::from_secs(30), future)
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
        self.enabled = enabled;
        if !enabled {
            self.audio.lock().unwrap().take();
            self.client.stop();
        }
    }
    async fn authenticate(&mut self, input: &Value) -> Result<AuthStatus, TuneError> {
        if !self.enabled {
            return Err("Spotify native is disabled".into());
        }
        if input == &json!({"poll": true}) {
            // A disconnected GET must neither spawn a child nor open discovery.
            if !self.client.alive() && !self.has_credentials() {
                return Ok(self.auth_status().await);
            }
            return self.call(Operation::Status).await;
        }
        if input != &json!({"device_flow": true}) && input.as_object().is_none_or(|o| !o.is_empty())
        {
            return Err("Spotify native uses app pairing; send an empty object".into());
        }
        self.call(Operation::Pair).await
    }
    async fn auth_status(&self) -> AuthStatus {
        let mut status = self.client.snapshot.lock().unwrap().status.clone();
        status.authenticated &= self.enabled && self.client.alive();
        status.verification_url = Some("/api/v1/streaming/spotify/native-pairing".into());
        status
    }
    fn auth_details(&self) -> Option<Value> {
        Some(self.pairing_status())
    }
    fn credential_key(&self) -> String {
        "auth_tokens_spotify_native".into()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        self.audio.lock().unwrap().take();
        self.client.stop();
        let mut snapshot = self.client.snapshot.lock().unwrap();
        snapshot.tokens["credentials"] = Value::Null;
        snapshot.status = AuthStatus::default();
        snapshot.details = json!({"pairing": false, "error": null});
        snapshot.dirty = true;
        Ok(())
    }
    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        self.call(Operation::Search {
            query: query.into(),
            limit,
        })
        .await
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
        self.call(Operation::Track { id: id.into() }).await
    }
    async fn get_track_url(&self, _: &str, _: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(unsupported("public audio URL; play through a Tune zone"))
    }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, TuneError> {
        self.call(Operation::Album { id: id.into() }).await
    }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.call(Operation::AlbumTracks { id: id.into() }).await
    }
    async fn get_artist(&self, id: &str) -> Result<StreamArtist, TuneError> {
        self.call(Operation::Artist { id: id.into() }).await
    }
    async fn get_artist_albums(&self, id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        self.call(Operation::ArtistAlbums { id: id.into() }).await
    }
    async fn get_playlist(&self, id: &str) -> Result<StreamPlaylist, TuneError> {
        self.call(Operation::Playlist { id: id.into() }).await
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.call(Operation::PlaylistTracks { id: id.into() }).await
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.call(Operation::UserPlaylists).await
    }
    async fn get_playlist_library(&self) -> Result<PlaylistLibrary, TuneError> {
        self.call(Operation::PlaylistLibrary).await
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        self.call(Operation::UserTracks).await
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Err(unsupported("saved albums"))
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Err(unsupported("followed artists"))
    }
    fn save_tokens(&self) -> Option<Value> {
        Some(self.client.tokens())
    }
    fn restore_tokens(&mut self, tokens: &Value) -> bool {
        // Deserializing credentials creates no Session and performs no I/O.
        if tokens["kind"] != TOKEN_KIND
            || !tokens["credentials"].is_object()
            || tokens["device_id"]
                .as_str()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .is_none()
        {
            return false;
        }
        let Ok(credentials) = serde_json::from_value::<librespot_core::authentication::Credentials>(
            tokens["credentials"].clone(),
        ) else {
            return false;
        };
        if credentials.auth_data.is_empty() {
            return false;
        }
        self.client.snapshot.lock().unwrap().tokens = tokens.clone();
        true
    }
    async fn post_restore(&mut self) {
        if self.enabled && self.has_credentials() {
            let _ = self.call::<AuthStatus>(Operation::Status).await;
        }
    }
    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        if self.enabled && self.client.alive() {
            let _: AuthStatus = self.call(Operation::Status).await?;
        }
        Ok(std::mem::take(
            &mut self.client.snapshot.lock().unwrap().dirty,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_proxy_poll_and_logout_do_not_spawn_a_worker() {
        let mut service = SpotifyNativeService::new();
        assert!(
            !service
                .authenticate(&json!({"poll": true}))
                .await
                .unwrap()
                .authenticated
        );
        assert!(!service.client.alive());
        assert!(
            service
                .authenticate(&json!({"password": "fixture"}))
                .await
                .is_err()
        );
        assert_eq!(service.credential_key(), "auth_tokens_spotify_native");
        assert!(!service.restore_tokens(&json!({"access_token": "web-fixture"})));
        service.logout().await.unwrap();
        assert!(service.save_tokens().unwrap()["credentials"].is_null());
    }
}
