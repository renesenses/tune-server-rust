//! #9449 (fil 3033, rc13.1) — en mode « 30 s », le préchargement de la piste
//! suivante était consommé puis jeté dès qu'elle durait plus de 30 s
//! (`prefetch_skip_truncated_serving_full_stream`), quelle que soit la
//! sortie. Et pour le préparer, Tune téléchargeait la piste ENTIÈRE à pleine
//! vitesse puis la décodait en entier (le tampon n'est tronqué qu'après), au
//! début de chaque morceau.
//!
//! Le banc : un faux Qobuz dont l'adresse de piste pointe sur un serveur qui
//! compte ses connexions. Contre-épreuve : avant le correctif, une piste de
//! 144 s y ouvre une connexion et laisse un tampon (test rouge).
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::prefetch::{PrefetchEngine, PrefetchMode, tampon_30s_servable};
use crate::streaming::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;

fn wav_silencieux(ms: u32) -> Vec<u8> {
    let sr: u32 = 44_100;
    let data = vec![0u8; (sr as usize * 4 * ms as usize) / 1000];
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sr.to_le_bytes());
    wav.extend_from_slice(&(sr * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

fn cdn_qui_compte(corps: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let n = Arc::new(AtomicUsize::new(0));
    let compte = n.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for flux in listener.incoming() {
            let Ok(mut s) = flux else { break };
            compte.fetch_add(1, Ordering::SeqCst);
            let corps = corps.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = s.write_all(&corps);
            });
        }
    });
    (format!("http://127.0.0.1:{port}/piste.wav"), n)
}

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien n'est servi".into())
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
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Ok(StreamUrl {
            url: self.url.clone(),
            mime_type: "audio/wav".into(),
            quality: StreamQuality {
                codec: "wav".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: vec![],
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

fn titre(source_id: &str, duration_ms: i64) -> QueueInput {
    QueueInput::Streaming {
        media_format: None,
        sample_rate: None,
        bit_depth: None,
        source: "qobuz".into(),
        source_id: source_id.into(),
        title: format!("Titre {source_id}"),
        artist: "Quelqu'un".into(),
        album: Some("Un album".into()),
        cover_url: None,
        duration_ms,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

/// Mode « 30 s » (le défaut), zone UPnP qui joue `A` ; la suivante dure
/// `duree_suivante_ms`. Rend le nombre de connexions au CDN et la présence
/// d'un tampon après le préchargement.
async fn precharger(duree_suivante_ms: i64, corps: Vec<u8>) -> (usize, bool) {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("dlna"), Some("uuid:banc-9449"))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[titre("A", 200_000), titre("B", duree_suivante_ms)],
        )
        .unwrap();
    let playback = Arc::new(PlaybackManager::new());
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "Titre A".into(),
                source: "qobuz".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    playback.update_queue_info(zone_id, 0, 2).await;
    let (url, connexions) = cdn_qui_compte(corps);
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxQobuz { url }));
    let moteur = PrefetchEngine::new();
    moteur
        .prefetch_next(db, Arc::new(Mutex::new(registre)), playback, zone_id)
        .await;
    let tampon = moteur.take_prefetched("qobuz", "B").await.is_some();
    (connexions.load(Ordering::SeqCst), tampon)
}

/// Le témoin de #9449 : une suivante de 144 s (celle du journal) n'est plus
/// téléchargée pour un tampon de 30 s promis à être jeté.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pas_de_prechargement_jete_pour_une_piste_de_plus_de_30_s() {
    let (connexions, tampon) = precharger(144_000, wav_silencieux(2_000)).await;
    assert_eq!(
        connexions, 0,
        "la suivante de 144 s a été téléchargée pour un tampon de 30 s que la \
         lecture jette toujours (#9449)"
    );
    assert!(!tampon);
}

/// Le préchargement garde son office pour une piste qui tient dans le tampon :
/// elle est téléchargée une fois et servie depuis la mémoire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_piste_courte_est_toujours_prechargee() {
    let (connexions, tampon) = precharger(2_000, wav_silencieux(2_000)).await;
    assert_eq!(connexions, 1);
    assert!(tampon, "une piste de 2 s tient dans le tampon de 30 s");
}

/// La règle est celle de la lecture (`prefetch_buffer_truncated`) : durée
/// inconnue jetée, ≤ 32 s gardée ; le mode « complet » n'est pas touché.
#[test]
fn la_regle_suit_celle_de_la_lecture() {
    assert!(!tampon_30s_servable(PrefetchMode::Buffer30s, 144_000));
    assert!(!tampon_30s_servable(PrefetchMode::Buffer30s, 0));
    assert!(tampon_30s_servable(PrefetchMode::Buffer30s, 30_000));
    assert!(tampon_30s_servable(PrefetchMode::Buffer30s, 32_000));
    assert!(!tampon_30s_servable(PrefetchMode::Buffer30s, 32_001));
    assert!(tampon_30s_servable(PrefetchMode::Full, 600_000));
}
