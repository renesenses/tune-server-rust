//! Fil 2121 — la lecture d'un album Bandcamp range la PAGE de l'album avec
//! chaque piste de la file (`queue_items.album_ref`, migration 114).
//!
//! C'est l'entrée en file qui compte : la page est la seule source d'une
//! signature fraîche quand l'URL de flux, qui EST le `source_id` d'une piste
//! Bandcamp, expire (410 au bout de quelques jours). Sans elle en file, le
//! relais n'a rien pour resigner (voir `tune-stream-http/tests/
//! bandcamp_resigne_par_sa_page_2121.rs` pour la resignature elle-même).
//!
//! Contre-épreuve : la route qui écrit la file par `set_streaming_queue` (sans
//! les références) laisse `album_ref` NULL, et ce test rougit sur la première
//! assertion.

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const PAGE: &str = "https://framewerk.bandcamp.com/album/love-parade";

/// Un faux Bandcamp : la page rend deux pistes. Les URL de flux pointent sur
/// un port fermé — rien ne doit dépendre du réseau ici.
struct FauxBandcamp;

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le faux Bandcamp ne sert pas : {quoi}"))
}

fn piste(n: u32) -> StreamTrack {
    StreamTrack {
        id: format!("http://127.0.0.1:9/stream/e43be2a9/mp3-128/{n}?ts=1790782809"),
        title: format!("Piste {n}"),
        artist: "Framewerk".into(),
        album: Some("Love Parade".into()),
        album_id: Some(PAGE.into()),
        duration_ms: 300_000,
        cover_path: None,
        track_number: Some(n),
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for FauxBandcamp {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "bandcamp"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(non_prevu("authenticate"))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_prevu("search"))
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_prevu("get_track"))
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_prevu("get_track_url"))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        if album_id != PAGE {
            return Err(TuneError::NotFound(album_id.to_string()));
        }
        Ok(vec![piste(1), piste(2)])
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_prevu("get_artist"))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_prevu("get_playlist"))
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_playlist_tracks"))
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn la_lecture_d_un_album_bandcamp_range_sa_page_avec_chaque_piste() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.services.lock().await.register(Box::new(FauxBandcamp));
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();

    let app = router().with_state(state.clone());
    let corps = serde_json::json!({
        "source": "bandcamp",
        "streaming_album_id": PAGE,
        "start_index": 0,
    });
    let rep = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{zid}/play"))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let _ = to_bytes(rep.into_body(), 1024 * 1024).await;

    // La file est écrite AVANT la lecture : quoi qu'il advienne de celle-ci
    // sur ce banc sans sortie, la file doit porter la page.
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert_eq!(
        file.len(),
        2,
        "les deux pistes de la page (statut {statut})"
    );
    for e in &file {
        assert_eq!(
            e.album_ref.as_deref(),
            Some(PAGE),
            "la piste {:?} doit garder la page de son album",
            e.title
        );
    }
}
