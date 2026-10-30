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
//! `dsp_progressif_reseau`), un faux Qobuz qui rend un FLAC de 6 s servi par
//! `Range` depuis la boucle locale, une sortie factice de type `dlna` qui note
//! les `play_media` et les `seek`, et une piste en lecture sur un canal sans
//! `Range`.
//!
//! Contre-épreuve : remettre dans `deplacer_la_sortie` la recréation sans
//! `seek_ms` suivie du `Seek` SOAP fait tomber
//! `avancer_dans_un_titre_qobuz_converti_recree_le_flux_a_la_position_7422`
//! (le `Seek` part, et la nouvelle session sert les 6 s depuis le début).
use super::PlaybackOrchestrator;
use super::service_wav_progressif_5080::{flux_qobuz, zone_du_fil_1949};
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
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
use axum::{
    Router,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// L'appareil de la zone du fil 1949 (`zone_du_fil_1949`).
const APPAREIL: &str = "uuid:c19-5080";
const SR: u32 = 44_100;
const SECONDES: u32 = 6;
const CIBLE_MS: u64 = 3_000;
/// Octets de PCM d'une seconde : 44,1 kHz, 16 bits, stéréo.
const OCTETS_PAR_SECONDE: usize = (SR * 4) as usize;

/// Un FLAC de [`SECONDES`] s, encodé par Tune.
async fn flac_de_six_secondes() -> Vec<u8> {
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    // Un signal non nul, pour que rien ne se résume à du silence compressé.
    let mut pcm = Vec::with_capacity((SR * SECONDES * 4) as usize);
    for i in 0..(SR * SECONDES) {
        let v = (((i % 100) as i16) - 50) * 100;
        let o = v.to_le_bytes();
        pcm.extend_from_slice(&[o[0], o[1], o[0], o[1]]);
    }
    enc.write(&pcm).await.unwrap();
    enc.finish().await.unwrap()
}

/// Un « CDN » qui sert `octets` en `Range`, comme Qobuz.
async fn cdn_par_range(octets: Vec<u8>) -> String {
    let octets = Arc::new(octets);
    let app = Router::new().route(
        "/piste.flac",
        get(move |entetes: HeaderMap| {
            let octets = octets.clone();
            async move {
                let total = octets.len();
                let (debut, fin) = entetes
                    .get(header::RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes="))
                    .and_then(|v| {
                        let (a, b) = v.split_once('-')?;
                        let a: usize = a.parse().ok()?;
                        let b: usize = if b.is_empty() {
                            total - 1
                        } else {
                            b.parse().ok()?
                        };
                        Some((a, b.min(total - 1)))
                    })
                    .unwrap_or((0, total - 1));
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (
                            header::CONTENT_RANGE,
                            format!("bytes {debut}-{fin}/{total}"),
                        ),
                        (header::CONTENT_TYPE, "audio/flac".to_string()),
                    ],
                    octets[debut..=fin].to_vec(),
                )
                    .into_response()
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    url
}

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
async fn titre_qobuz_converti_en_lecture() -> (PlaybackOrchestrator, i64, String) {
    let url = cdn_par_range(flac_de_six_secondes().await).await;
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
    // Témoin sur la LONGUEUR servie : la compensation de niveau (#5071) n'y
    // change rien, on la coupe pour garder le banc simple.
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_level_compensation"), "false")
        .unwrap();
    let sortie = MockOutput::new(APPAREIL, "DDC-0 C19").with_type("dlna");
    orch.outputs.lock().await.register(Box::new(sortie));

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

/// Octets de PCM servis par la session, en-tête WAV exclu.
async fn pcm_servi(orch: &PlaybackOrchestrator, sid: &str) -> usize {
    let session = {
        let etat = orch.streamer.sessions_state();
        let etat = etat.lock().await;
        etat.get(sid).expect("la session doit exister").clone()
    };
    let premier = tokio::time::timeout(Duration::from_secs(10), session.recv_chunk())
        .await
        .expect("aucun octet en 10 s")
        .expect("canal fermé avant l'en-tête");
    assert_eq!(&premier[..4], b"RIFF", "l'en-tête WAV doit passer intact");
    let mut total = premier.len().saturating_sub(44);
    while let Ok(Some(chunk)) =
        tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await
    {
        total += chunk.len();
    }
    total
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avancer_dans_un_titre_qobuz_converti_recree_le_flux_a_la_position_7422() {
    let (orch, zone_id, sid) = titre_qobuz_converti_en_lecture().await;
    assert!(
        !orch.streamer.is_seekable_session(&sid).await,
        "prémisse : le titre est servi par un canal, sans Range"
    );

    orch.seek(zone_id, CIBLE_MS, Some(APPAREIL))
        .await
        .expect("le déplacement doit aboutir");
    // Laisser partir un éventuel Seek détaché d'après relecture.
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;

    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(
        etat.position_ms, CIBLE_MS as i64,
        "la position suit le geste"
    );
    let nouveau = etat
        .now_playing
        .and_then(|np| np.stream_id)
        .expect("une session porte le flux");
    assert_ne!(nouveau, sid, "le flux a été recréé");
    assert!(
        !orch.streamer.is_seekable_session(&nouveau).await,
        "prémisse : le flux recréé repart lui aussi par la voie conversion, sans Range"
    );
    let (lectures, seeks) = recu_par_la_sortie(&orch).await;
    assert_eq!(lectures, 1, "le flux recréé a été posé sur le renderer");
    assert!(
        seeks.is_empty(),
        "aucun Seek SOAP ne doit partir vers un flux sans Range, qu'il ne peut \
         pas honorer : {seeks:?}"
    );

    // Le flux recréé doit COMMENCER à la position : il sert le reste du
    // titre (3 s), pas ses 6 s depuis le début.
    let servi = pcm_servi(&orch, &nouveau).await;
    let restant = OCTETS_PAR_SECONDE * (SECONDES as usize - (CIBLE_MS / 1000) as usize);
    assert!(
        servi > OCTETS_PAR_SECONDE,
        "trop peu de PCM servi : {servi} octets"
    );
    assert!(
        servi <= restant + OCTETS_PAR_SECONDE / 2,
        "le flux recréé sert {servi} octets, soit le titre depuis le début \
         (attendu ≈ {restant} octets, le reste après {CIBLE_MS} ms) : le \
         renderer rejouerait le morceau au début (fil 2535)"
    );
}
