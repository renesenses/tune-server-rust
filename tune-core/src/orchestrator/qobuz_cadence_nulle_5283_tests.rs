//! #5283 — Qobuz sur sortie locale : `getFileUrl` rend `sampling_rate: 0`
//! pour un FLAC hi-res bien réel (Didier, fil 2000). Le WAV de la sortie
//! locale partait avec une cadence cible de 0, le décodeur refusait
//! (« stream target sample rate must be greater than zero »), la session
//! restait vide et l'écran accusait le fichier.
//!
//! Témoins par la porte publique (`resolve_stream`), sans réseau : la réponse
//! Qobuz est SIMULÉE (le JSON passe par le vrai lecteur,
//! `QobuzService::flux_de_get_file_url`) et le « CDN » est un serveur HTTP en
//! boucle locale qui sert un vrai FLAC 24/96.
use crate::TuneError;
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::streaming::qobuz::QobuzService;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// La réponse `getFileUrl` du fil 2000 : FLAC, cadence et profondeur à 0.
fn reponse_qobuz_nulle(url: &str) -> serde_json::Value {
    serde_json::json!({
        "track_id": 434679964,
        "duration": 180,
        "url": url,
        "format_id": 27,
        "mime_type": "audio/flac",
        "sampling_rate": 0,
        "bit_depth": 0,
    })
}

struct Serveur {
    url: String,
    _tache: tokio::task::JoinHandle<()>,
}

impl Drop for Serveur {
    fn drop(&mut self) {
        self._tache.abort();
    }
}

/// Un « CDN » qui sert un corps fixe à toute requête (200, sans `Range`).
async fn serveur(corps: Vec<u8>) -> Serveur {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", listener.local_addr().unwrap());
    let corps = Arc::new(corps);
    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let entete = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Serveur { url, _tache: tache }
}

/// Un faux Qobuz : `get_track_url` rend la réponse simulée, lue par le vrai
/// lecteur de `qobuz.rs` ; `get_track` rend le catalogue voulu.
struct FauxQobuz {
    reponse: serde_json::Value,
    catalogue: Option<StreamQuality>,
}

#[async_trait::async_trait]
impl StreamingService for FauxQobuz {
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
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        match &self.catalogue {
            Some(q) => Ok(StreamTrack {
                id: id.to_string(),
                title: "How Could I Be Such A Fool".into(),
                artist: "The Mothers Of Invention".into(),
                album: None,
                album_id: None,
                duration_ms: 180_000,
                cover_path: None,
                track_number: Some(6),
                disc_number: None,
                explicit: false,
                quality: Some(q.clone()),
                isrc: None,
                disponible: None,
                composer: None,
                artist_id: None,
            }),
            None => Err("catalogue indisponible".into()),
        }
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Ok(QobuzService::flux_de_get_file_url(&self.reponse)?)
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

fn demande(zone_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some("local:Sortie SMSL SU-8".into()),
        source: Some("qobuz".into()),
        source_id: Some("434679964".into()),
        title: Some("How Could I Be Such A Fool".into()),
        ..Default::default()
    }
}

async fn preparer(
    corps: Vec<u8>,
    catalogue: Option<StreamQuality>,
) -> (PlaybackOrchestrator, i64, Serveur) {
    let srv = serveur(corps).await;
    let orch = orchestrateur();
    orch.services.lock().await.register(Box::new(FauxQobuz {
        reponse: reponse_qobuz_nulle(&srv.url),
        catalogue,
    }));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("SMSL", Some("local"), Some("local:Sortie SMSL SU-8"))
        .unwrap();
    (orch, zone_id, srv)
}

/// Tous les octets d'une session, jusqu'à la fin du flux (ou 5 s de silence
/// après les premiers octets).
async fn octets_de_la_session(orch: &PlaybackOrchestrator, sid: &str) -> Vec<u8> {
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite");
    let mut octets = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), session.recv_chunk()).await {
            Ok(Some(c)) => octets.extend_from_slice(&c),
            Ok(None) | Err(_) => break,
        }
    }
    octets
}

fn flac_24_96() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flac/ref_24_96000_stereo.flac"
    ))
    .unwrap()
}

/// Le fait du fil 2000, tel que `qobuz.rs` le lit : le zéro de Qobuz arrive
/// tel quel dans la qualité (le repli 44,1/16 ne vaut que pour un champ
/// absent). C'est ce que l'orchestrateur doit savoir remplacer.
#[test]
fn qobuz_rend_une_cadence_nulle_telle_quelle_5283() {
    let flux = QobuzService::flux_de_get_file_url(&reponse_qobuz_nulle("https://cdn/x")).unwrap();
    assert_eq!((flux.quality.sample_rate, flux.quality.bit_depth), (0, 0));
}

/// Le témoin du défaut : cadence annoncée 0, sortie locale. La piste doit
/// jouer à la cadence que l'EN-TÊTE du FLAC énonce (96 kHz), pas partir à 0
/// — ni à une cadence inventée.
#[tokio::test]
async fn une_cadence_qobuz_nulle_joue_a_la_cadence_de_l_en_tete_du_flac_5283() {
    let (orch, zone_id, _srv) = preparer(flac_24_96(), None).await;
    let r = orch
        .resolve_stream(&demande(zone_id))
        .await
        .expect("la piste se résout");
    let sid = r
        .stream_id
        .as_deref()
        .expect("session WAV de la sortie locale");
    let octets = octets_de_la_session(&orch, sid).await;
    assert!(
        octets.len() > 44,
        "session vide ({} octets) : le transcodage est parti à une cadence de 0 et le \
         décodeur l'a refusé (« stream target sample rate must be greater than zero »)",
        octets.len()
    );
    assert_eq!(
        &octets[0..4],
        b"RIFF",
        "la session commence par un en-tête WAV"
    );
    let cadence = u32::from_le_bytes([octets[24], octets[25], octets[26], octets[27]]);
    assert_eq!(
        cadence, 96_000,
        "le WAV servi doit porter la cadence de l'en-tête FLAC (96 kHz), il porte {cadence}"
    );
}

/// En-tête illisible, catalogue muet : l'erreur est NOMMÉE et aucune session
/// n'est ouverte — pas de transcodage lancé à 0.
#[tokio::test]
async fn cadence_introuvable_erreur_nommee_sans_session_5283() {
    let (orch, zone_id, _srv) = preparer(b"ceci n'est pas un FLAC".repeat(64), None).await;
    let avant = orch.streamer.sessions_state().lock().await.len();
    let erreur = orch
        .resolve_stream(&demande(zone_id))
        .await
        .err()
        .expect("une cadence inconnue doit être refusée à la résolution");
    assert!(
        erreur.contains(crate::streaming::cadence_du_flux::CODE_CADENCE_INCONNUE),
        "erreur non nommée : {erreur}"
    );
    assert_eq!(
        orch.streamer.sessions_state().lock().await.len(),
        avant,
        "aucune session WAV ne doit être ouverte pour une cadence inconnue"
    );
}

/// En-tête illisible, catalogue renseigné : la cadence du catalogue sert de
/// cible — la résolution ne part pas à 0.
#[tokio::test]
async fn sans_en_tete_le_catalogue_donne_la_cadence_cible_5283() {
    let catalogue = StreamQuality {
        codec: "FLAC".into(),
        sample_rate: 192_000,
        bit_depth: 24,
        bitrate: None,
        channels: 2,
    };
    let (orch, zone_id, _srv) =
        preparer(b"ceci n'est pas un FLAC".repeat(64), Some(catalogue)).await;
    let r = orch
        .resolve_stream(&demande(zone_id))
        .await
        .expect("le catalogue donne la cadence");
    assert!(r.stream_id.is_some(), "session WAV de la sortie locale");
    let sessions = orch.streamer.sessions_state();
    let sessions = sessions.lock().await;
    let s = sessions.get(r.stream_id.as_deref().unwrap()).unwrap();
    assert_eq!(s.info.sample_rate, 192_000, "cadence cible du catalogue");
}
