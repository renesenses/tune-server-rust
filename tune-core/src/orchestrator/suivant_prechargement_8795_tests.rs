//! #8795 — `resolve_streaming_url` passe bien à l'abandon du préchargement
//! le verdict `decoder_pendant_le_telechargement(req)` de la sortie.
//!
//! Le faux TIDAL : son premier `get_track_url` (le préchargement de la piste
//! « LENTE ») ne rend jamais la main, comme un téléchargement DASH sans fin,
//! en tenant le service en écriture ; les suivants échouent tout de suite.
//! Sur une sortie locale, la lecture de LENTE abandonne ce préchargement et
//! rend sa réponse aussitôt ; sur un renderer DLNA, elle le reprend (#7247)
//! et attend donc le service.
//!
//! Contre-épreuve : passer `false` au lieu de
//! `decoder_pendant_le_telechargement(req)` dans `resolve_streaming_url` fait
//! tomber `sortie_locale_ne_reste_pas_derriere_le_prechargement_8795`.
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::playback::NowPlaying;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct FauxTidal {
    appels: AtomicUsize,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for FauxTidal {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        Err(non_servi())
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
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Err(non_servi())
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_servi())
    }
    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        let rang = self.appels.fetch_add(1, Ordering::SeqCst);
        if rang == 0 {
            // Le préchargement : un téléchargement DASH qui n'en finit pas.
            std::future::pending::<()>().await;
        }
        let _ = (track_id, quality);
        Err(non_servi())
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
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

fn titre(source_id: &str) -> QueueInput {
    QueueInput::Streaming {
        media_format: None,
        sample_rate: None,
        bit_depth: None,
        source: "tidal".into(),
        source_id: source_id.into(),
        title: format!("Titre {source_id}"),
        artist: "Quelqu'un".into(),
        album: Some("Un album".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

/// Une zone sur `appareil` qui joue `A`, file `[A, LENTE]`, et le
/// préchargement de LENTE en vol chez le faux service.
async fn banc(appareil: &str) -> (PlaybackOrchestrator, PlayRequest) {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let mut registre = crate::streaming::registry::ServiceRegistry::new();
    registre.register(Box::new(FauxTidal {
        appels: AtomicUsize::new(0),
    }));
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(registre)),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        Some("127.0.0.1".into()),
    );
    let type_de_sortie = appareil.split(':').next().unwrap_or("local");
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Banc 8795", Some(type_de_sortie), Some(appareil))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(zone_id, &[titre("A"), titre("LENTE")])
        .unwrap();
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Titre A".into(),
                source: "tidal".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_queue_info(zone_id, 0, 2).await;
    {
        let (moteur, db, services, playback) = (
            orch.prefetch.clone(),
            orch.db.clone(),
            orch.services.clone(),
            orch.playback.clone(),
        );
        tokio::spawn(async move { moteur.prefetch_next(db, services, playback, zone_id).await });
    }
    // Le préchargement est chez le service, qu'il tient en écriture.
    let svc = orch.services.lock().await.get("tidal").expect("faux Tidal");
    let mut tenu = false;
    for _ in 0..200 {
        if tokio::time::timeout(Duration::from_millis(5), svc.write())
            .await
            .is_err()
        {
            tenu = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(tenu, "prémisse : le préchargement de LENTE tient le service");
    let req = PlayRequest {
        zone_id,
        output_device_id: Some(appareil.into()),
        source: Some("tidal".into()),
        source_id: Some("LENTE".into()),
        title: Some("Titre LENTE".into()),
        duration_ms: Some(200_000),
        ..Default::default()
    };
    (orch, req)
}

#[tokio::test]
async fn sortie_locale_ne_reste_pas_derriere_le_prechargement_8795() {
    let (orch, req) = banc("local:Casque").await;
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        orch.resolve_streaming_url("tidal", &req),
    )
    .await;
    assert!(
        r.is_ok(),
        "sortie locale : la lecture de la piste préchargée attend encore la fin \
         du téléchargement du préchargement (#8795)"
    );
}

#[tokio::test]
async fn renderer_dlna_reprend_le_prechargement_de_la_piste_8795() {
    let (orch, req) = banc("dlna:uuid:banc-8795").await;
    let r = tokio::time::timeout(
        Duration::from_millis(500),
        orch.resolve_streaming_url("tidal", &req),
    )
    .await;
    assert!(
        r.is_err(),
        "renderer réseau : le préchargement de la piste demandée est repris (#7247), \
         la lecture l'attend"
    );
}
