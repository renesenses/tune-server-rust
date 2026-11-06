//! #7422 — fil 2535 (Tades, 1.0.0-rc8.1 Tune OS, Qobuz vers le DDC-0 C19 en
//! DLNA, Chrome) : un déplacement dans le morceau le relance AU DÉBUT, pendant
//! que l'écran affiche la position demandée.
//!
//! Journal du rapport : la piste Qobuz part par la voie `conversion` (WAV
//! progressif, traitement de zone actif, #5080), servie par un canal sans
//! `Range`. Au déplacement : `seek_streaming_complete position_ms=3202`, un
//! `Seek` SOAP acquitté en 1 ms, puis le renderer relu à `position_ms=1000`.
//! `deplacer_la_sortie` recréait le flux SANS position (`seek_ms: None`), donc
//! depuis l'octet 0, puis comptait sur un `Seek` qu'un flux sans `Range` ne
//! peut pas honorer.
//!
//! Le banc : la zone du fil 1949 (même C19, égaliseur armé, opt-in
//! `dsp_progressif_reseau`), un faux Qobuz qui rend une adresse HTTPS de CDN
//! (le bras `resoudre_flux_https`, donc le WAV progressif de #5080 ; une
//! adresse `http://` serait servie telle quelle, sans session), une sortie factice de type `dlna` qui note les
//! `play_media` et les `seek`, et une piste en lecture sur un canal sans
//! `Range`.
//!
//! Contre-épreuve : remettre dans `deplacer_la_sortie` la recréation sans
//! `seek_ms` suivie du `Seek` SOAP fait tomber
//! `avancer_dans_un_titre_qobuz_converti_recree_le_flux_a_la_position_7422`
//! (un `Seek` part vers le flux sans `Range`).
use super::PlaybackOrchestrator;
use super::service_wav_progressif_5080::{flux_qobuz, zone_du_fil_1949};
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::error::TuneError;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// L'appareil de la zone du fil 1949 (`zone_du_fil_1949`).
const APPAREIL: &str = "uuid:c19-5080";
const SR: u32 = 44_100;
const SECONDES: u32 = 6;
const CIBLE_MS: u64 = 3_000;

/// Une adresse HTTPS de CDN qui refuse la connexion, comme le banc de #2095 :
/// la résolution prend le bras HTTPS sans attendre, la tâche de décodage
/// échoue derrière, sans rien changer à ce que le déplacement envoie au
/// renderer.
const CDN: &str = "https://127.0.0.1:9/file/441697196.flac";

/// Un faux Qobuz : `get_track_url` rend l'adresse du CDN de banc.
struct QobuzDuBanc {
    url: String,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien d'autre n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for QobuzDuBanc {
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
        Err(non_servi())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Ok(flux_qobuz(self.url.clone(), SR, 16))
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

/// La zone C19 qui joue un titre Qobuz converti, servi par un canal sans
/// `Range` (la voie `conversion` du rapport).
async fn titre_qobuz_converti_en_lecture(url: String) -> (PlaybackOrchestrator, i64, String) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzDuBanc { url }));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = zone_du_fil_1949(&orch);
    let sortie = MockOutput::new(APPAREIL, "DDC-0 C19").with_type("dlna");
    orch.outputs.lock().await.register(Box::new(sortie));
    // La sonde SOAP du LPCM, que la sortie factice ne sait pas faire : le
    // C19 accepte le WAV 16 bits (fil 1949), d'où la voie conversion.
    orch.dlna_lpcm_accepte
        .lock()
        .await
        .insert(format!("{APPAREIL}|16"), true);

    let (sid, _tx, _pret) = orch
        .streamer
        .create_session(
            StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: SR,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            false,
            256,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "V. Songe d'une nuit du sabbat".into(),
                source: "qobuz".into(),
                source_id: Some("441697196".into()),
                stream_id: Some(sid.clone()),
                duration_ms: SECONDES as i64 * 1000,
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id, sid)
}

/// `(play_media reçus, cibles des seek reçus)`.
async fn recu_par_la_sortie(orch: &PlaybackOrchestrator) -> (usize, Vec<u64>) {
    let arc = { orch.outputs.lock().await.get(APPAREIL) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.seek_calls())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avancer_dans_un_titre_qobuz_converti_recree_le_flux_a_la_position_7422() {
    let (orch, zone_id, sid) = titre_qobuz_converti_en_lecture(CDN.into()).await;
    assert!(
        !orch.streamer.is_seekable_session(&sid).await,
        "prémisse : le titre est servi par un canal, sans Range"
    );

    let resultat = tokio::time::timeout(
        Duration::from_secs(10),
        orch.seek(zone_id, CIBLE_MS, Some(APPAREIL)),
    )
    .await;
    // Laisser partir un éventuel Seek détaché d'après relecture.
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
    let etat = orch.playback.get_state(zone_id).await;
    let (lectures, seeks) = recu_par_la_sortie(&orch).await;
    let nouveau = etat
        .now_playing
        .as_ref()
        .and_then(|np| np.stream_id.clone());
    let nouveau_cherchable = match nouveau.as_deref() {
        Some(s) => Some(orch.streamer.is_seekable_session(s).await),
        None => None,
    };

    resultat
        .expect("le déplacement ne doit pas rester suspendu")
        .expect("le déplacement doit aboutir");
    assert_eq!(
        etat.position_ms, CIBLE_MS as i64,
        "la position suit le geste"
    );
    let nouveau = nouveau.expect("une session porte le flux recréé");
    assert_ne!(nouveau, sid, "le flux a été recréé");
    assert_eq!(
        nouveau_cherchable,
        Some(false),
        "prémisse : le flux recréé repart lui aussi par la voie conversion, sans Range"
    );
    // Le CDN de banc refuse la connexion : la session ne reçoit jamais son
    // premier octet, et la pose sur le renderer n'a pas lieu (`lectures`
    // reste à 0, avant comme après le correctif). Le témoin porte sur ce que
    // le déplacement envoie au renderer : aucun Seek.
    let _ = lectures;
    assert!(
        seeks.is_empty(),
        "aucun Seek SOAP ne doit partir vers un flux sans Range, qu'il ne peut \
         pas honorer : le renderer rejouerait le morceau au début (fil 2535) : \
         {seeks:?}"
    );
}
