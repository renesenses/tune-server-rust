//! Fil 2095 (FabienM, 1.0.0-rc1 Linux, Devialet Phantom « Salon » en DLNA) :
//! une reprise qui RÉTABLIT une session morte repartait à 0:00 au lieu de la
//! position de la pause.
//!
//! Journal, 02/10 07:12:46.889Z : `resume_stream_session_restore zone_id=6
//! position_ms=35016`, puis `proxy_session_created`, `SetAVTransportURI`,
//! `Play`, et le Phantom ouvre `range="-"` : aucun `Seek`. La requête de
//! rétablissement porte bien `seek_ms` (`requete_de_retablissement`), mais une
//! session MANDATAIRE sert depuis l'octet 0 et ne lit jamais `seek_ms` — la
//! règle de [`super::session::replay_needs_output_seek`] (#2893). La relecture
//! (`replay_zone_at_position`) envoyait ce `Seek` ; le rétablissement de
//! `resume`, non.
//!
//! Le banc : le VRAI `resume`, une zone DLNA dont la session de piste a été
//! ramassée pendant la pause, un service « qobuz » qui rend une URL HTTPS de
//! CDN (donc le bras mandataire de `relayer_le_flux`), et une sortie factice
//! de type `dlna` qui note les `seek` reçus.
//!
//! Contre-épreuve : retirer l'appel à `seek_output_after_replay` du bras
//! `RetablirALaPosition` de `resume` fait tomber
//! `la_reprise_d_une_session_mandataire_morte_ramene_le_renderer_a_la_pause`.
use super::PlaybackOrchestrator;
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:uuid-42566ad5-salon-2095";
/// La position conservée par la pause, journal de FabienM.
const POSITION_PAUSE_MS: u64 = 35_016;
const DUREE_MS: u64 = 302_973;

/// Un service « qobuz » qui rend une URL HTTPS signée, comme le CDN Akamai.
/// Rien ne la télécharge dans ce banc : la sortie factice ne tire pas le flux.
struct QobuzDeBanc;

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien d'autre n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for QobuzDeBanc {
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
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Ok(StreamUrl {
            url: format!("https://127.0.0.1:9/file/{track_id}.flac"),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 192_000,
                bit_depth: 24,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: Vec::new(),
        })
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

/// Zone DLNA en pause à [`POSITION_PAUSE_MS`] sur une piste Qobuz, dont la
/// session mandataire a été ramassée pendant la pause.
async fn zone_en_pause_session_morte(refus_de_seek: Option<&str>) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzDeBanc));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(APPAREIL))
        .unwrap();
    let sortie = MockOutput::new(APPAREIL, "Salon").with_type("dlna");
    sortie.refuser_le_seek(refus_de_seek);
    orch.outputs.lock().await.register(Box::new(sortie));
    let sid = orch
        .streamer
        .create_proxy_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 192_000,
                bit_depth: 24,
                channels: 2,
                ..Default::default()
            },
            "https://127.0.0.1:9/file/24293033.flac".into(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Tell Me a Bedtime Story".into(),
                source: "qobuz".into(),
                source_id: Some("24293033".into()),
                stream_id: Some(sid.clone()),
                duration_ms: DUREE_MS as i64,
                ..Default::default()
            },
        )
        .await;
    orch.playback
        .update_position(zone_id, POSITION_PAUSE_MS as i64)
        .await;
    orch.playback.pause(zone_id).await;
    // Le ramasse-miettes est passé pendant la pause.
    orch.streamer.remove_session(&sid).await;
    assert!(!orch.streamer.session_alive(&sid).await);
    (orch, zone_id)
}

/// `(play_media reçus, cibles des seek reçus, position de l'appareil)`.
async fn etat_de_la_sortie(orch: &PlaybackOrchestrator) -> (usize, Vec<u64>, u64) {
    let arc = { orch.outputs.lock().await.get(APPAREIL) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (
        mock.play_call_count().await,
        mock.seek_calls(),
        guard.get_status().await.unwrap().position_ms,
    )
}

/// Laisser partir la tâche détachée du Seek d'après relecture.
async fn laisser_passer_la_pose() {
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
}

/// LE défaut du fil 2095 : la session est rétablie sur une nouvelle session
/// mandataire, servie depuis l'octet 0. Le renderer doit recevoir la position
/// de la pause, sinon il repart à 0:00.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_reprise_d_une_session_mandataire_morte_ramene_le_renderer_a_la_pause() {
    let (orch, zone_id) = zone_en_pause_session_morte(None).await;
    orch.resume(zone_id, Some(APPAREIL))
        .await
        .expect("le rétablissement doit aboutir");
    laisser_passer_la_pose().await;

    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    let sid = np.stream_id.expect("une nouvelle session porte le flux");
    assert!(
        orch.streamer.is_seekable_session(&sid).await,
        "prémisse : le rétablissement passe par une session mandataire (servie depuis 0)"
    );
    let (lectures, seeks, position) = etat_de_la_sortie(&orch).await;
    assert_eq!(lectures, 1, "prémisse : le rétablissement a joué la piste");
    assert_eq!(
        seeks,
        vec![POSITION_PAUSE_MS],
        "le renderer doit recevoir UN Seek à la position de la pause"
    );
    assert_eq!(
        position, POSITION_PAUSE_MS,
        "le renderer repart à 0:35, pas à 0:00"
    );
    assert_eq!(
        orch.playback.get_state(zone_id).await.position_ms,
        POSITION_PAUSE_MS as i64
    );
}

/// Un renderer qui REFUSE le Seek (701) ne casse pas la reprise : la piste
/// joue, depuis le début — l'ancien comportement, et rien de pire.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_seek_refuse_ne_casse_pas_la_reprise() {
    let (orch, zone_id) =
        zone_en_pause_session_morte(Some("UPnP error 701: Transition not available")).await;
    orch.resume(zone_id, Some(APPAREIL))
        .await
        .expect("un Seek refusé ne doit pas faire échouer la reprise");
    laisser_passer_la_pose().await;
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "la zone joue"
    );
    let (lectures, seeks, _) = etat_de_la_sortie(&orch).await;
    assert_eq!(lectures, 1, "la piste a été jouée");
    assert_eq!(
        seeks,
        vec![POSITION_PAUSE_MS],
        "le Seek a été tenté une fois"
    );
}
