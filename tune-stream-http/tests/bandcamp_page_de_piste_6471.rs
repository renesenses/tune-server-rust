//! #6471 (FabienM, fil 2302 point 17) — jouer la version Bandcamp d'un titre
//! depuis « Autres versions » échouait sur le Chromecast : « Output device
//! error: load media: … Failed to load media ».
//!
//! La recherche de pistes Bandcamp des « Autres versions »
//! (`tune_bandcamp::rechercher_pistes`) rend pour `source_id` l'adresse de la
//! PAGE de la piste (`https://<artiste>.bandcamp.com/track/<titre>`), pas une
//! URL de flux `bcbits …/stream/…`. Le résolveur la prenait pour l'URL audio
//! (« le `source_id` d'une piste Bandcamp EST l'URL ») : la sortie recevait
//! du HTML.
//!
//! La règle : une page `/track/` se lit (comme une page d'album, elle porte
//! UNE piste), son flux du jour part vers la sortie, et la page devient la
//! référence qui permet de le resigner plus tard. Une page illisible est un
//! refus DIT, jamais une adresse HTML poussée à la sortie.
//!
//! Banc : base SQLite migrée, faux service « bandcamp », faux bcbits local.

use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::outputs::mock::MockOutput;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

/// L'adresse que rend la recherche Bandcamp pour « Even Thieves Couldn't Lie ».
const PAGE: &str = "https://exemple.bandcamp.com/track/even-thieves-couldnt-lie";
/// Une page que le faux service ne sait pas lire.
const PAGE_MORTE: &str = "https://exemple.bandcamp.com/track/retiree";

fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Un faux bcbits qui sert 200 `audio/mpeg` à tout.
async fn faux_bcbits() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/stream", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let corps = vec![0u8; 4_096];
                let entete = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

struct FauxBandcamp {
    flux: String,
    lectures: Arc<StdMutex<Vec<String>>>,
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le faux Bandcamp ne sert pas : {quoi}"))
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
    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
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
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Err(non_prevu("search"))
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_prevu("get_track"))
    }
    async fn get_track_url(
        &self,
        _track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(non_prevu("get_track_url"))
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    /// Une page de piste se lit comme une page d'album : une seule piste.
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.lectures.lock().unwrap().push(album_id.to_string());
        if album_id != PAGE {
            return Err(TuneError::NotFound(format!("page inconnue : {album_id}")));
        }
        Ok(vec![StreamTrack {
            id: self.flux.clone(),
            title: "Even Thieves Couldn't Lie".into(),
            artist: "Exemple".into(),
            album: Some("Even Thieves Couldn't Lie".into()),
            album_id: Some(PAGE.into()),
            duration_ms: 240_000,
            cover_path: None,
            track_number: Some(1),
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        }])
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_prevu("get_artist"))
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_prevu("get_playlist"))
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
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

const SORTIE_CAST: &str = "chromecast-enfants";

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    flux: String,
    lectures: Arc<StdMutex<Vec<String>>>,
}

async fn banc() -> Banc {
    let base = faux_bcbits().await;
    let flux = format!(
        "{base}/e43be2a9/mp3-128/29192493?p=0&ts={}&t=abcd&token=x",
        maintenant()
    );
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let lectures = Arc::new(StdMutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxBandcamp {
        flux: flux.clone(),
        lectures: Arc::clone(&lectures),
    }));
    let mut sorties = OutputRegistry::new();
    sorties.register(Box::new(
        MockOutput::new(SORTIE_CAST, "Enfants").with_type("chromecast"),
    ));
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(registre)),
        Arc::new(Mutex::new(sorties)),
        None,
    );
    Banc {
        orch,
        db,
        flux,
        lectures,
    }
}

impl Banc {
    /// Une zone à sortie LOCALE, la page en tête de file — comme une version
    /// ajoutée à la file depuis le panneau.
    fn zone_locale(&self, nom: &str, source_id: &str) -> i64 {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .create(nom, Some("local"), Some("local:hw0"))
            .unwrap();
        PlayQueueRepo::with_backend(self.db.clone())
            .append(
                zone,
                &[QueueInput::Streaming {
                    source: "bandcamp".into(),
                    source_id: source_id.into(),
                    title: "Even Thieves Couldn't Lie".into(),
                    artist: "Exemple".into(),
                    album: None,
                    cover_url: None,
                    duration_ms: 240_000,
                    track_number: None,
                    disc_number: None,
                    album_ref: None,
                    artist_ref: None,
                }],
            )
            .unwrap();
        zone
    }
}

#[tokio::test]
async fn une_page_de_piste_est_lue_et_son_flux_part_vers_la_sortie_locale() {
    let b = banc().await;
    let z = b.zone_locale("Bureau", PAGE);
    let r = b
        .orch
        .resolve_queue_item_url(z, 0)
        .await
        .expect("résolution");
    assert_eq!(
        r.url, b.flux,
        "#6471 : la sortie doit recevoir le flux de la piste, pas l'adresse de sa page"
    );
    assert_eq!(b.lectures.lock().unwrap().as_slice(), [PAGE]);
}

#[tokio::test]
async fn une_page_de_piste_jouee_sur_le_chromecast_part_sur_son_flux() {
    let b = banc().await;
    let z = ZoneRepo::with_backend(b.db.clone())
        .create("Enfants", Some("chromecast"), Some(SORTIE_CAST))
        .unwrap();
    // Exactement le corps du panneau « Autres versions » (`corpsVersionService`).
    let res = b
        .orch
        .play(PlayRequest {
            zone_id: z,
            output_device_id: Some(SORTIE_CAST.into()),
            source: Some("bandcamp".into()),
            source_id: Some(PAGE.into()),
            title: Some("Even Thieves Couldn't Lie".into()),
            artist_name: Some("Exemple".into()),
            ..Default::default()
        })
        .await
        .expect("lecture");
    assert!(
        res.output_sent,
        "la sortie a reçu la lecture : {:?}",
        res.error
    );
    let sid = res
        .stream_url
        .as_deref()
        .and_then(|u| u.rsplit('/').next())
        .and_then(|f| f.split('.').next())
        .expect("URL de session du relais")
        .to_string();
    let session = b
        .orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session de relais");
    assert_eq!(
        session.proxy_url.lock().await.as_deref(),
        Some(b.flux.as_str()),
        "#6471 : le relais du Chromecast doit tirer le flux bcbits, pas la page HTML"
    );
}

#[tokio::test]
async fn une_page_illisible_est_un_refus_dit_et_rien_ne_part() {
    let b = banc().await;
    let z = b.zone_locale("Chambre", PAGE_MORTE);
    let e = match b.orch.resolve_queue_item_url(z, 0).await {
        Ok(r) => panic!(
            "une page illisible ne doit pas partir vers la sortie : {}",
            r.url
        ),
        Err(e) => e,
    };
    assert!(e.contains("page Bandcamp"), "le refus nomme la cause : {e}");
}
