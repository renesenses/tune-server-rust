//! Fil 2121 — resigner une URL Bandcamp AVANT de l'envoyer à la sortie
//! (décision de Bertrand, 04/10 : toutes les sorties).
//!
//! La sortie locale et OAAT ouvrent l'URL bcbits elles-mêmes : le relais, qui
//! sait guérir un 410 (#5706), n'est pas sur leur chemin. La règle : avant de
//! transmettre l'URL, lire l'âge de sa signature (`ts`) ; à 24 h ou plus,
//! relire la page connue et substituer l'URL du jour ; sans page, ou si la
//! relecture échoue, garder l'URL d'origine et le DIRE (WARN).
//!
//! Un seul `#[tokio::test]` : l'abonné de journal est global au binaire.
//!
//! Banc : base SQLite migrée, faux service « bandcamp » (la page rend les URL
//! du jour), faux bcbits local qui consigne chaque requête. Les signatures sont
//! datées par rapport à MAINTENANT : vieille = il y a 2,7 jours (le cas de
//! Fabien), récente = il y a une minute.

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
use tune_core::event_bus::EventBus;
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

const PAGE: &str = "https://framewerk.bandcamp.com/album/love-parade";

#[derive(Clone, Default)]
struct JournalCapture(Arc<StdMutex<Vec<u8>>>);

impl JournalCapture {
    fn depuis(&self, repere: usize) -> String {
        let brut = self.0.lock().unwrap();
        String::from_utf8_lossy(&brut[repere.min(brut.len())..]).into_owned()
    }
    fn repere(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn maintenant() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn signature(ts: u64) -> String {
    format!("p=0&ts={ts}&t=abcd&token={ts}_x")
}

/// Un faux bcbits qui sert 200 `audio/mpeg` à tout et CONSIGNE la première
/// ligne de chaque requête.
async fn faux_bcbits(requetes: Arc<StdMutex<Vec<String>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/stream", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requetes = Arc::clone(&requetes);
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let ligne = String::from_utf8_lossy(&requete)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                requetes.lock().unwrap().push(ligne);
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

fn piste(id: String) -> StreamTrack {
    StreamTrack {
        id,
        title: "Meet Her At The Love Parade (Framewerk Rewerk)".into(),
        artist: "Framewerk".into(),
        album: Some("Love Parade".into()),
        album_id: Some(PAGE.into()),
        duration_ms: 300_000,
        cover_path: None,
        track_number: Some(2),
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

/// Le faux service Bandcamp : relire la page rend les URL du jour.
struct FauxBandcamp {
    base: String,
    signature_du_jour: String,
    relectures: Arc<StdMutex<Vec<String>>>,
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
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.relectures.lock().unwrap().push(album_id.to_string());
        if album_id != PAGE {
            return Err(TuneError::NotFound(format!("page inconnue : {album_id}")));
        }
        Ok(vec![
            piste(format!(
                "{}/aaaa1111/mp3-128/11111111?{}",
                self.base, self.signature_du_jour
            )),
            piste(format!(
                "{}/e43be2a9/mp3-128/29192493?{}",
                self.base, self.signature_du_jour
            )),
        ])
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

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    base: String,
    signature_du_jour: String,
    relectures: Arc<StdMutex<Vec<String>>>,
    requetes: Arc<StdMutex<Vec<String>>>,
}

const SORTIE_RESEAU: &str = "uuid:beosound-parents";

async fn banc() -> Banc {
    let requetes = Arc::new(StdMutex::new(Vec::new()));
    let base = faux_bcbits(Arc::clone(&requetes)).await;
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let relectures = Arc::new(StdMutex::new(Vec::new()));
    let signature_du_jour = signature(maintenant());
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxBandcamp {
        base: base.clone(),
        signature_du_jour: signature_du_jour.clone(),
        relectures: Arc::clone(&relectures),
    }));
    let mut sorties = OutputRegistry::new();
    sorties.register(Box::new(
        MockOutput::new(SORTIE_RESEAU, "Parents").with_type("dlna"),
    ));
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(registre)),
        Arc::new(Mutex::new(sorties)),
        None,
    );
    // Le bus d'évènements est ce qui autorise la sonde de niveaux.
    orch.event_bus = Some(Arc::new(EventBus::new()));
    Banc {
        orch,
        db,
        base,
        signature_du_jour,
        relectures,
        requetes,
    }
}

impl Banc {
    fn url(&self, ts: u64) -> String {
        format!("{}/e43be2a9/mp3-128/29192493?{}", self.base, signature(ts))
    }
    fn fraiche(&self) -> String {
        format!(
            "{}/e43be2a9/mp3-128/29192493?{}",
            self.base, self.signature_du_jour
        )
    }
    /// Une zone dont la sortie est `sortie`, et la piste en tête de file.
    fn zone_avec(&self, nom: &str, sortie: &str, source_id: &str, page: Option<&str>) -> i64 {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .create(nom, Some("local"), Some(sortie))
            .unwrap();
        PlayQueueRepo::with_backend(self.db.clone())
            .append(
                zone,
                &[QueueInput::Streaming {
                    source: "bandcamp".into(),
                    source_id: source_id.into(),
                    title: "Meet Her At The Love Parade".into(),
                    artist: "Framewerk".into(),
                    album: None,
                    cover_url: None,
                    duration_ms: 300_000,
                    track_number: None,
                    disc_number: None,
                    album_ref: page.map(String::from),
                }],
            )
            .unwrap();
        zone
    }
    fn relectures(&self) -> usize {
        self.relectures.lock().unwrap().len()
    }
}

#[tokio::test]
async fn une_signature_vieille_est_renouvelee_avant_l_envoi_a_toute_sortie() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne).expect("abonné global libre");

    let b = banc().await;
    let vieille = b.url(maintenant() - 233_000); // 2,7 jours : le cas de Fabien
    let recente = b.url(maintenant() - 60);

    // 1. Sortie LOCALE, signature vieille, page connue : la sortie reçoit
    //    l'URL du jour (elle l'ouvre elle-même, aucun relais pour guérir).
    let z = b.zone_avec("Bureau", "local:hw0", &vieille, Some(PAGE));
    let r = b
        .orch
        .resolve_queue_item_url(z, 0)
        .await
        .expect("résolution");
    assert_eq!(r.url, b.fraiche(), "la sortie locale reçoit l'URL resignée");
    assert_eq!(b.relectures(), 1, "une relecture de la page");

    // 2. Sortie RÉSEAU (lecture complète) : la session de relais part sur
    //    l'URL du jour, et la sonde de niveaux lit l'URL du jour.
    let repere_req = b.requetes.lock().unwrap().len();
    let z = b.zone_avec("Parents", SORTIE_RESEAU, &vieille, Some(PAGE));
    let res = b
        .orch
        .play(PlayRequest {
            zone_id: z,
            output_device_id: Some(SORTIE_RESEAU.into()),
            source: Some("bandcamp".into()),
            source_id: Some(vieille.clone()),
            title: Some("Meet Her At The Love Parade".into()),
            duration_ms: Some(300_000),
            album_ref: Some(PAGE.into()),
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
        .expect("URL de session")
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
        Some(b.fraiche().as_str()),
        "le relais part sur l'URL resignée"
    );
    // La sonde de niveaux tire l'amont en tâche de fond : on attend sa requête.
    let attente = std::time::Instant::now();
    loop {
        let vues: Vec<String> = b.requetes.lock().unwrap()[repere_req..].to_vec();
        if vues.iter().any(|l| l.contains(&b.signature_du_jour)) {
            assert!(
                !vues
                    .iter()
                    .any(|l| l.contains("ts=") && !l.contains(&b.signature_du_jour)),
                "aucune requête ne doit partir sur la signature vieille : {vues:?}"
            );
            break;
        }
        assert!(
            attente.elapsed() < std::time::Duration::from_secs(10),
            "la sonde de niveaux doit lire l'URL du jour ; requêtes vues : {vues:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // 3. Signature RÉCENTE : rien ne change, aucune relecture.
    let avant = b.relectures();
    let z = b.zone_avec("Cuisine", "local:hw1", &recente, Some(PAGE));
    let r = b
        .orch
        .resolve_queue_item_url(z, 0)
        .await
        .expect("résolution");
    assert_eq!(r.url, recente, "une signature récente part telle quelle");
    assert_eq!(
        b.relectures(),
        avant,
        "aucune relecture pour une signature récente"
    );

    // 4. Signature vieille SANS page connue : URL d'origine, et le journal le dit.
    let repere = capture.repere();
    let autre = format!(
        "{}/ffff/mp3-128/77777777?{}",
        b.base,
        signature(maintenant() - 233_000)
    );
    let z = b.zone_avec("Chambre", "local:hw2", &autre, None);
    let r = b
        .orch
        .resolve_queue_item_url(z, 0)
        .await
        .expect("résolution");
    assert_eq!(r.url, autre, "sans page, l'URL d'origine");
    assert_eq!(b.relectures(), avant, "rien à relire");
    let journal = capture.depuis(repere);
    let ligne = journal
        .lines()
        .find(|l| l.contains("bandcamp_signature_perimee_sans_reference"))
        .unwrap_or_else(|| panic!("le cas doit être DIT au journal :\n{journal}"));
    assert!(ligne.contains("WARN"), "un avertissement : {ligne}");
}
