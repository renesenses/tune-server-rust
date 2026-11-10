//! #8229 — fil 2727 (Régis, rc11, macOS) : dans une playlist importée en M3U,
//! un titre Qobuz atteint par l'ENCHAÎNEMENT arrive dans le lecteur et dans
//! « Lecture en cours » sans pochette ni artiste ; le même titre lancé au clic
//! les a.
//!
//! La ligne de file d'un titre Qobuz importé ne porte que le titre et la
//! durée du M3U. Au clic, la résolution directe complète la fiche auprès du
//! service (#7738, `resolve_streaming_url`). À l'enchaînement, le titre
//! suivant a été préchargé : il est servi par `serve_prefetched_pcm`, dont
//! l'étiquetage (`etiqueter_le_prefetch`, `dsp.rs`) ne consultait le service
//! que pour un titre VIDE. Un titre présent suffisait à servir une fiche sans
//! artiste, album ni pochette.
//!
//! Contre-épreuve : dans `etiqueter_le_prefetch`, retirer `|| fiche_incomplete`
//! de la condition fait tomber les deux témoins `prefetch_8229_*` (artiste et
//! pochette à `None`).
use super::*;
use crate::error::TuneError;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ID_QOBUZ: &str = "441697196";
const RENDERER: &str = "dlna:renderer-8229";
const POCHETTE: &str = "https://static.qobuz.com/images/covers/8229_600.jpg";

/// Un faux Qobuz qui connaît la fiche complète du titre.
struct QobuzQuiConnaitLaFiche;

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien d'autre n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for QobuzQuiConnaitLaFiche {
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
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
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
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_servi())
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        Ok(StreamTrack {
            id: id.into(),
            title: "Foule sentimentale".into(),
            artist: "Alain Souchon".into(),
            album: Some("C'est déjà ça".into()),
            album_id: Some("0825646".into()),
            duration_ms: 1_000,
            cover_path: Some(POCHETTE.into()),
            track_number: Some(1),
            disc_number: Some(1),
            explicit: false,
            quality: None,
            isrc: None,
            disponible: None,
            composer: None,
            artist_id: None,
        })
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_servi())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
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

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzQuiConnaitLaFiche));
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// La demande que `play_from_queue` construit pour la ligne de file d'un
/// titre Qobuz importé d'un M3U : le titre et la durée du fichier, rien
/// d'autre.
fn demande_m3u(sortie: Option<&str>) -> PlayRequest {
    PlayRequest {
        zone_id: 1,
        output_device_id: sortie.map(Into::into),
        track_id: None,
        source: Some("qobuz".into()),
        source_id: Some(ID_QOBUZ.into()),
        title: Some("Foule sentimentale".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(1_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

/// Le tampon préchargé pour ce titre : il a reçu le même titre seul.
fn tampon_m3u() -> crate::prefetch::PrefetchedTrack {
    crate::prefetch::PrefetchedTrack {
        source: "qobuz".into(),
        source_id: ID_QOBUZ.into(),
        stream_id: None,
        // Une seconde de PCM 16 bits stéréo à 44,1 kHz.
        pcm_data: vec![0u8; 44_100 * 4],
        format: "wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        duration_ms: 1_000,
        title: "Foule sentimentale".into(),
        artist: None,
        album: None,
        cover_url: None,
        mime_type: "audio/wav".into(),
        upstream_url: None,
    }
}

/// Sortie réseau (DLNA) : le préchargement est encodé en fichier.
#[tokio::test]
async fn prefetch_8229_titre_qobuz_m3u_enchaine_vers_un_renderer_a_sa_fiche() {
    let orch = orchestrateur();
    let r = orch
        .serve_prefetched_pcm(tampon_m3u(), &demande_m3u(Some(RENDERER)))
        .await
        .expect("le préchargement doit être servi");
    assert_eq!(r.title, "Foule sentimentale", "le titre importé est gardé");
    assert_eq!(
        r.artist.as_deref(),
        Some("Alain Souchon"),
        "titre Qobuz enchaîné sans artiste"
    );
    assert_eq!(
        r.cover_url.as_deref(),
        Some(POCHETTE),
        "titre Qobuz enchaîné sans pochette"
    );
    assert_eq!(r.album.as_deref(), Some("C'est déjà ça"));
}

/// Hors sortie réseau : le préchargement est servi en WAV.
#[tokio::test]
async fn prefetch_8229_titre_qobuz_m3u_enchaine_hors_reseau_a_sa_fiche() {
    let orch = orchestrateur();
    let r = orch
        .serve_prefetched_pcm(tampon_m3u(), &demande_m3u(None))
        .await
        .expect("le préchargement doit être servi");
    assert_eq!(r.artist.as_deref(), Some("Alain Souchon"));
    assert_eq!(r.cover_url.as_deref(), Some(POCHETTE));
    assert_eq!(r.album.as_deref(), Some("C'est déjà ça"));
}

/// Témoin : une fiche Qobuz déjà complète n'est pas remplacée.
#[tokio::test]
async fn prefetch_8229_fiche_complete_gardee_telle_quelle() {
    let orch = orchestrateur();
    let mut demande = demande_m3u(Some(RENDERER));
    demande.artist_name = Some("Interprète de la file".into());
    demande.album_title = Some("Album de la file".into());
    demande.cover_url = Some("https://exemple.invalid/file.jpg".into());
    let r = orch
        .serve_prefetched_pcm(tampon_m3u(), &demande)
        .await
        .expect("le préchargement doit être servi");
    assert_eq!(r.artist.as_deref(), Some("Interprète de la file"));
    assert_eq!(r.album.as_deref(), Some("Album de la file"));
    assert_eq!(
        r.cover_url.as_deref(),
        Some("https://exemple.invalid/file.jpg")
    );
}

/// Relecture de #8229 — un Qobuz qui ne répond pas : `get_track` signale son
/// entrée puis ne rend jamais la main.
struct QobuzMuet {
    entre: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl StreamingService for QobuzMuet {
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
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
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
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_servi())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        self.entre.notify_one();
        std::future::pending().await
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_servi())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
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

/// Relecture de #8229 — pendant que Qobuz tarde à compléter la fiche, le
/// registre des services reste libre (les autres services ne sont pas
/// bloqués), et la lecture part sans la fiche au bout du délai.
///
/// Contre-épreuve : tenir le registre pendant `get_track` (l'ancien
/// `let registry = self.services.lock().await;`) fait tomber l'assertion
/// `try_lock` ; retirer le `timeout` fait pendre le test.
#[tokio::test(start_paused = true)]
async fn prefetch_8229_service_lent_ne_bloque_ni_le_registre_ni_la_lecture() {
    let entre = Arc::new(tokio::sync::Notify::new());
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzMuet {
        entre: entre.clone(),
    }));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );

    let debut = tokio::time::Instant::now();
    let demande = demande_m3u(None);
    let servi = orch.serve_prefetched_pcm(tampon_m3u(), &demande);
    let sonde = async {
        entre.notified().await;
        assert!(
            orch.services.try_lock().is_ok(),
            "le registre des services est tenu pendant l'appel réseau get_track"
        );
    };
    let (r, ()) = tokio::join!(servi, sonde);
    let r = r.expect("le préchargement doit être servi sans la fiche");
    assert_eq!(r.title, "Foule sentimentale");
    assert_eq!(r.artist, None, "aucune fiche n'est arrivée");
    assert!(
        debut.elapsed() >= super::dsp::DELAI_FICHE_DU_PRECHARGEMENT,
        "la lecture n'a pas attendu le délai"
    );
}
