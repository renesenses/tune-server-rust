//! #8983 (fil 2890) — le saut d'une piste QOBUZ sur une sortie locale.
//!
//! La relance à la position (`replay_zone_at_position`) résout la piste avec
//! `seek_ms` : le WAV servi à la sortie locale doit COMMENCER à la position.
//! Le « CDN » est un serveur en boucle locale qui honore `Range`, comme celui
//! de Qobuz, et sert un FLAC dont chaque seconde porte une valeur distincte.
//! Aucun réseau, aucune sortie audio : la session WAV est lue directement.
use crate::TuneError;
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::streaming::qobuz::QobuzService;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SR: u32 = 44_100;
const SECONDES: u32 = 6;
const MARQUE: i16 = 1000;

async fn flac_marque() -> Vec<u8> {
    let mut pcm = Vec::with_capacity((SR * SECONDES * 4) as usize);
    for s in 0..SECONDES {
        let v = (MARQUE * (s as i16 + 1)).to_le_bytes();
        for _ in 0..SR * 2 {
            pcm.extend_from_slice(&v);
        }
    }
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&pcm).await.unwrap();
    enc.finish().await.unwrap()
}

struct Cdn {
    url: String,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for Cdn {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

/// Un CDN qui honore `Range: bytes=N-` (206) et sert le corps entier sinon.
async fn cdn(corps: Vec<u8>) -> Cdn {
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
                let texte = String::from_utf8_lossy(&requete).to_ascii_lowercase();
                let debut = texte
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.split('-').next())
                    .and_then(|n| n.trim().parse::<usize>().ok());
                let total = corps.len();
                let (statut, depuis) = match debut {
                    Some(d) if d < total => ("206 Partial Content", d),
                    _ => ("200 OK", 0),
                };
                let mut entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\n\
                     Content-Length: {}\r\nConnection: close\r\n",
                    total - depuis
                );
                if depuis > 0 || debut.is_some() {
                    entete.push_str(&format!(
                        "Content-Range: bytes {depuis}-{}/{total}\r\n",
                        total - 1
                    ));
                }
                entete.push_str("\r\n");
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps[depuis..]).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Cdn { url, tache }
}

struct FauxQobuz {
    url: String,
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
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err("catalogue indisponible".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Ok(QobuzService::flux_de_get_file_url(&serde_json::json!({
            "track_id": 898300,
            "duration": SECONDES,
            "url": self.url,
            "format_id": 6,
            "mime_type": "audio/flac",
            "sampling_rate": 44.1,
            "bit_depth": 16,
        }))?)
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

/// Les octets PCM de la session (après le bloc `data`) et sa profondeur.
async fn pcm_de_la_session(orch: &PlaybackOrchestrator, sid: &str) -> (Vec<u8>, u16) {
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite");
    let mut octets = Vec::new();
    while let Ok(Some(c)) = tokio::time::timeout(Duration::from_secs(5), session.recv_chunk()).await
    {
        octets.extend_from_slice(&c);
    }
    assert_eq!(&octets[..4], b"RIFF", "flux WAV");
    let bits = u16::from_le_bytes([octets[34], octets[35]]);
    let data = octets.windows(4).position(|w| w == b"data").unwrap() + 8;
    (octets[data..].to_vec(), bits)
}

fn seconde_de(pcm: &[u8], i: usize, bits: u16) -> i64 {
    let largeur = usize::from(bits / 8);
    let b = &pcm[i..i + largeur];
    let v: i32 = match largeur {
        2 => i32::from(i16::from_le_bytes([b[0], b[1]])),
        3 => i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 16,
        _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 16,
    };
    (f64::from(v) / f64::from(MARQUE)).round() as i64 - 1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_saut_qobuz_sur_sortie_locale_part_de_la_position_8983() {
    let cdn = cdn(flac_marque().await).await;
    let orch = orchestrateur();
    orch.services.lock().await.register(Box::new(FauxQobuz {
        url: cdn.url.clone(),
    }));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Pink Faun", Some("local"), Some("local:Pink Faun"))
        .unwrap();
    for depart_ms in [0u64, 3000] {
        let r = orch
            .resolve_stream(&PlayRequest {
                zone_id,
                output_device_id: Some("local:Pink Faun".into()),
                source: Some("qobuz".into()),
                source_id: Some("898300".into()),
                title: Some("Marque".into()),
                duration_ms: Some(i64::from(SECONDES) * 1000),
                seek_ms: (depart_ms > 0).then_some(depart_ms),
                ..Default::default()
            })
            .await
            .expect("la piste se résout");
        let sid = r.stream_id.as_deref().expect("session WAV");
        let (pcm, bits) = pcm_de_la_session(&orch, sid).await;
        let largeur = usize::from(bits / 8);
        let premiere = seconde_de(&pcm, 0, bits);
        let derniere = seconde_de(&pcm, pcm.len() - largeur, bits);
        let secondes_servies = pcm.len() / (SR as usize * 2 * largeur);
        assert_eq!(
            (premiere, derniere, secondes_servies),
            (
                (depart_ms / 1000) as i64,
                i64::from(SECONDES) - 1,
                (SECONDES as u64 - depart_ms / 1000) as usize
            ),
            "Qobuz, départ à {depart_ms} ms : le flux de la sortie locale doit commencer \
             à la position et finir à la fin réelle"
        );
    }
}
