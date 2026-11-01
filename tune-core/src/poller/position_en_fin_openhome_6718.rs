//! #6718 : fin OpenHome puis résolution lente de la suivante.
//!
//! Le vrai tick reçoit cinq relevés Stopped/0 après 359 s sur une piste de
//! 360 s. Au cinquième, la monotonie cédait AVANT la décision de fin ; puis
//! handle_track_end attendait la résolution de B avec A encore Playing à 0.
//! Le service de banc bloque cette résolution par notification, sans délai
//! ni appareil. Le journal réel ne contient pas ces relevés : le témoin
//! reproduit le mécanisme visuel, pas une mesure du LUXMAN ni de Qobuz.
//! La sonde LPCM et le consentement de #6795 restent inchangés.

use super::*;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "uuid:temoin-openhome-6718";

/// Banc minimal : une zone, une sortie factice dont on pose la position à la
/// main, et le vrai `tick` appelé tour par tour avec un état de sondage qui
/// SURVIT d'un tour à l'autre (sans quoi le changement de génération ne serait
/// jamais détecté).
struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("openhome"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Témoin 6718").with_type("openhome"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            playback,
            outputs,
            zone_id,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
        }
    }

    /// Lancer une piste : c'est le geste de l'auditeur qui clique.
    async fn jouer(&mut self, track_id: i64, titre: &str, duree_ms: i64) {
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    track_id: Some(track_id),
                    title: titre.into(),
                    source: "local".into(),
                    duration_ms: duree_ms,
                    ..Default::default()
                },
            )
            .await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        self.poll_states
            .entry(self.zone_id)
            .or_insert_with(|| ZonePollState::new(generation));
    }

    /// Ce que la SORTIE rend au sondeur ce tour-ci.
    async fn renderer(&self, etat: TransportState, position_ms: u64, duree_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(duree_ms);
        mock.set_position(position_ms);
    }

    /// Reculer l'horloge de piste pour que le premier échantillon soit jugé
    /// plausible (`stale_start_position` compare à l'horloge murale).
    fn dater_le_debut(&mut self, il_y_a: Duration) {
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - il_y_a);
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Ce que `GET /zones` sert : `routes/zones/lecture.rs` compose
    /// `position_ms` et `current_track` depuis CE seul appel.
    async fn servi(&self) -> (i64, Option<i64>) {
        let etat = self.playback.get_state(self.zone_id).await;
        (
            etat.position_ms,
            etat.now_playing.and_then(|np| np.track_id),
        )
    }
}

// Diagnostic #6718 : hypothèse explicite, et non relevé du LUXMAN.
#[tokio::test]
async fn un_arret_openhome_sans_suivante_ne_repart_pas_au_debut_6718() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "Ancienne piste", 360_000).await;
    banc.dater_le_debut(Duration::from_secs(360));
    banc.renderer(TransportState::Playing, 359_000, 360_000)
        .await;
    banc.tic().await;
    assert_eq!(banc.servi().await.0, 359_000);
    for tour in 0..8 {
        banc.renderer(TransportState::Stopped, 0, 360_000).await;
        banc.tic().await;
        let etat = banc.playback.get_state(banc.zone_id).await;
        assert!(
            etat.state != crate::playback::PlayState::Playing || etat.position_ms >= 350_000,
            "ancienne piste republiee en lecture depuis son debut : tour={tour}, position={}",
            etat.position_ms
        );
    }
}

use crate::error::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
struct ServiceBloque6718 {
    demandes: Arc<std::sync::Mutex<Vec<String>>>,
    entree: Arc<tokio::sync::Notify>,
    liberation: Arc<tokio::sync::Notify>,
}
fn non_servi() -> TuneError {
    TuneError::Streaming("fin volontaire du diagnostic 6718".into())
}
#[async_trait::async_trait]
impl StreamingService for ServiceBloque6718 {
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
        self.demandes.lock().unwrap().push(track_id.to_string());
        self.entree.notify_one();
        self.liberation.notified().await;
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

#[tokio::test]
async fn la_fin_openhome_ne_remet_pas_l_ancienne_piste_a_zero_pendant_la_resolution_6718() {
    use super::suivante_bannie_5143::titre_qobuz;
    use crate::db::play_queue_repo::PlayQueueRepo;
    let banc = Banc::monter().await;
    let orch = banc.poller.orchestrator.clone();
    PlayQueueRepo::with_backend(orch.db.clone())
        .append(banc.zone_id, &[titre_qobuz("A"), titre_qobuz("B")])
        .unwrap();
    banc.playback
        .play(
            banc.zone_id,
            NowPlaying {
                source: "qobuz".into(),
                source_id: Some("A".into()),
                title: "Titre A".into(),
                duration_ms: 360_000,
                ..Default::default()
            },
        )
        .await;
    banc.playback.update_queue_info(banc.zone_id, 0, 2).await;
    banc.playback.update_position(banc.zone_id, 359_000).await;
    let entree = Arc::new(tokio::sync::Notify::new());
    let liberation = Arc::new(tokio::sync::Notify::new());
    let demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
    orch.services
        .lock()
        .await
        .register(Box::new(ServiceBloque6718 {
            entree: entree.clone(),
            liberation: liberation.clone(),
            demandes: demandes.clone(),
        }));
    let etat = banc.playback.get_state(banc.zone_id).await;

    banc.renderer(TransportState::Stopped, 0, 360_000).await;
    let mut ps = ZonePollState::new(etat.track_generation);
    ps.track_started_at = Some(Instant::now() - Duration::from_secs(360));
    ps.track_loaded_at = Instant::now() - Duration::from_secs(360);
    ps.peak_position_ms = 359_000;
    ps.last_position_ms = 359_000;
    let mut polls = HashMap::from([(banc.zone_id, ps)]);
    let mut idle = HashMap::new();
    for _ in 0..STOPPED_TICKS_THRESHOLD - 1 {
        banc.poller
            .tick(&mut polls, &mut idle, &Instant::now())
            .await;
    }
    let now = Instant::now();
    let avance = banc.poller.tick(&mut polls, &mut idle, &now);
    tokio::pin!(avance);
    tokio::select! {
        _ = &mut avance => panic!("la resolution doit attendre"),
        _ = entree.notified() => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("la suivante n'est pas demandee"),
    }
    let pendant = banc.playback.get_state(banc.zone_id).await;
    assert!(pendant.resolving);
    assert_eq!(pendant.now_playing.unwrap().source_id.as_deref(), Some("A"));
    assert_eq!(
        pendant.position_ms, 359_000,
        "la resolution suivante a remis l'ancienne piste au debut"
    );
    assert_eq!(*demandes.lock().unwrap(), vec!["B"]);
    liberation.notify_one();
    tokio::time::timeout(Duration::from_secs(5), &mut avance)
        .await
        .unwrap();
}

#[tokio::test]
async fn une_position_openhome_qui_avance_meme_arretee_reste_publiee_6718() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "Piste A", 360_000).await;
    banc.dater_le_debut(Duration::from_secs(100));
    banc.playback.update_position(banc.zone_id, 97_000).await;
    banc.renderer(TransportState::Stopped, 98_000, 360_000)
        .await;
    banc.tic().await;
    assert_eq!(
        banc.servi().await.0,
        98_000,
        "ne pas masquer la progression qui dement un faux arret (#6717)"
    );
}

#[tokio::test]
async fn un_recul_openhome_en_lecture_reste_observable_6718() {
    let mut banc = Banc::monter().await;
    banc.jouer(1, "Piste A", 360_000).await;
    banc.dater_le_debut(Duration::from_secs(100));
    banc.playback.update_position(banc.zone_id, 97_000).await;
    for _ in 0..5 {
        banc.renderer(TransportState::Playing, 0, 360_000).await;
        banc.tic().await;
    }
    assert_eq!(
        banc.servi().await.0,
        0,
        "le filtre de fin ne doit pas masquer une position en lecture"
    );
}
