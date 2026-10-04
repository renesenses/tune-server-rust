//! Fil 2121 (partie A) — une URL Bandcamp expirée est RESIGNÉE depuis la page
//! de son album, que la piste soit rejouée depuis la file, un favori ou
//! l'historique.
//!
//! FabienM (1.0.0-rc1, zone Cast « Parents », 03/10) : la piste relancée
//! portait l'URL bcbits signée le 30/09 (`ts=1790782809`) ; Bandcamp a répondu
//! 410 Gone et le relais n'avait rien pour resigner. Le `source_id` d'une
//! piste Bandcamp EST l'URL signée, et l'adresse de la page — seule source
//! d'une signature fraîche — n'était rangée nulle part.
//!
//! Le banc assemble le chemin réel de bout en bout, sans réseau :
//!
//! - une base SQLite migrée (migration 114 : `album_ref` sur `queue_items`,
//!   `streaming_favorites`, `listen_history`) ;
//! - un orchestrateur dont le registre porte un faux service « bandcamp » :
//!   `get_album_tracks(page)` rend les URL du jour, comme une relecture de la
//!   page ;
//! - un faux bcbits local : 410 sur la signature du 30/09, 200 `audio/mpeg`
//!   sur la signature du jour ;
//! - la résolution de la file (`resolve_queue_item_url`, celle du gapless et
//!   du pré-chargement), puis le relais de `tune-stream-http`
//!   (`handle_stream`) tel qu'un renderer le tire.
//!
//! Contre-épreuve intégrée : une piste dont aucune table ne connaît la page
//! échoue comme avant (502), sans relire aucune page.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::history_repo::{HistoryRepo, ListenRecord};
use tune_core::db::migrations::run_migrations;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const PAGE: &str = "https://framewerk.bandcamp.com/album/love-parade";
const SIGNATURE_PERIMEE: &str = "p=0&ts=1790782809&t=dcd4&token=1790782809_perimee";
const SIGNATURE_DU_JOUR: &str = "p=0&ts=1791020000&t=f00d&token=1791020000_du_jour";

/// Un faux bcbits : 410 sur la signature du 30/09, 403 sans signature (le
/// chemin nu), 200 `audio/mpeg` sur la signature du jour. Rend la base
/// `http://hôte:port/stream` et le compteur de requêtes.
async fn faux_bcbits(corps: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/stream", listener.local_addr().unwrap());
    let requetes = Arc::new(AtomicUsize::new(0));
    let compte = Arc::clone(&requetes);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            let compte = Arc::clone(&compte);
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                compte.fetch_add(1, Ordering::SeqCst);
                let ligne = String::from_utf8_lossy(&requete)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let reponse: &[u8] = if ligne.contains("ts=1790782809") {
                    b"HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if !ligne.contains("ts=1791020000") {
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    let entete = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        corps.len()
                    );
                    let _ = socket.write_all(entete.as_bytes()).await;
                    let _ = socket.write_all(&corps).await;
                    let _ = socket.shutdown().await;
                    return;
                };
                let _ = socket.write_all(reponse).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, requetes)
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
                "{}/aaaa1111/mp3-128/11111111?{SIGNATURE_DU_JOUR}",
                self.base
            )),
            piste(format!(
                "{}/e43be2a9/mp3-128/29192493?{SIGNATURE_DU_JOUR}",
                self.base
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
    /// L'URL de la piste signée le 30/09, sur le faux bcbits.
    perimee: String,
    corps: Vec<u8>,
    relectures: Arc<StdMutex<Vec<String>>>,
    requetes_amont: Arc<AtomicUsize>,
}

async fn banc() -> Banc {
    let corps: Vec<u8> = (0..48_000).map(|i| (i % 251) as u8).collect();
    let (base, requetes_amont) = faux_bcbits(corps.clone()).await;
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let relectures = Arc::new(StdMutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxBandcamp {
        base: base.clone(),
        relectures: Arc::clone(&relectures),
    }));
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(registre)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    Banc {
        orch,
        db,
        perimee: format!("{base}/e43be2a9/mp3-128/29192493?{SIGNATURE_PERIMEE}"),
        corps,
        relectures,
        requetes_amont,
    }
}

impl Banc {
    /// Une zone RÉSEAU (ni `local:` ni `oaat:`) : Bandcamp y passe par le
    /// relais, comme vers la Beosound de Fabien.
    fn zone(&self, nom: &str) -> i64 {
        ZoneRepo::with_backend(self.db.clone())
            .create(nom, Some("dlna"), Some(&format!("uuid:{nom}")))
            .expect("zone")
    }

    fn file(&self) -> PlayQueueRepo {
        PlayQueueRepo::with_backend(self.db.clone())
    }

    fn entree(&self, source_id: &str, album_ref: Option<&str>) -> QueueInput {
        QueueInput::Streaming {
            source: "bandcamp".into(),
            source_id: source_id.into(),
            title: "Meet Her At The Love Parade (Framewerk Rewerk)".into(),
            artist: "Framewerk".into(),
            album: Some("Love Parade".into()),
            cover_url: None,
            duration_ms: 300_000,
            track_number: Some(2),
            disc_number: None,
            album_ref: album_ref.map(String::from),
        }
    }

    /// Résout la tête de file de `zone` puis tire le flux comme un renderer
    /// (`GET bytes=0-`). Rend le statut et les octets servis.
    async fn jouer_la_file(&self, zone: i64) -> (u16, Vec<u8>) {
        let r = self
            .orch
            .resolve_queue_item_url(zone, 0)
            .await
            .expect("résolution de la file");
        let sid = r
            .stream_id
            .expect("Bandcamp vers un renderer réseau passe par une session de relais");
        let mut h = axum::http::HeaderMap::new();
        h.insert("Range", "bytes=0-".parse().unwrap());
        let rep = tune_stream_http::handle_stream(
            Path(format!("{sid}.mp3")),
            State(self.orch.streamer.sessions_state()),
            h,
        )
        .await;
        let statut = rep.status().as_u16();
        let mut flux = rep.into_body().into_data_stream();
        let mut octets = Vec::new();
        while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
            .await
            .expect("le corps doit arriver")
        {
            octets.extend_from_slice(&bloc.expect("bloc lisible"));
        }
        (statut, octets)
    }

    fn relectures(&self) -> Vec<String> {
        self.relectures.lock().unwrap().clone()
    }
}

/// La file : la piste rangée avec sa page (lecture de l'album) guérit du 410.
#[tokio::test]
async fn une_piste_en_file_avec_sa_page_est_resignee_par_le_relais() {
    let b = banc().await;
    let zone = b.zone("Parents");
    b.file()
        .append(zone, &[b.entree(&b.perimee, Some(PAGE))])
        .expect("file");
    assert_eq!(
        b.file()
            .get_at(zone, 0)
            .unwrap()
            .unwrap()
            .album_ref
            .as_deref(),
        Some(PAGE),
        "la file rend la page qu'on y a rangée (migration 114)"
    );

    let (statut, octets) = b.jouer_la_file(zone).await;

    assert!(
        matches!(statut, 200 | 206),
        "410 sur la signature du 30/09, puis la page relue : la piste est servie (statut {statut})"
    );
    assert_eq!(
        octets, b.corps,
        "les octets audio du jour, relayés tels quels"
    );
    assert_eq!(
        b.relectures(),
        vec![PAGE.to_string()],
        "UNE relecture, de la page rangée avec la piste"
    );
    assert!(
        b.requetes_amont.load(Ordering::SeqCst) >= 2,
        "l'amont a vu l'URL périmée PUIS l'URL du jour"
    );
}

/// Le favori : posé après l'écoute de l'album, il hérite de la page ; rejoué
/// tel que la liste des favoris le rend, une fois la file vidée, il guérit.
#[tokio::test]
async fn un_favori_rejoue_apres_la_file_videe_est_resigne() {
    let b = banc().await;
    let ecoute = b.zone("Salon");
    b.file()
        .append(ecoute, &[b.entree(&b.perimee, Some(PAGE))])
        .expect("file de l'écoute");
    let favoris = StreamingFavoritesRepo::with_backend(b.db.clone());
    favoris
        .add(
            1,
            "track",
            "bandcamp",
            &b.perimee,
            Some("Meet Her At The Love Parade"),
            Some("Framewerk"),
            None,
            None,
        )
        .expect("favori");
    let rendu = favoris.list(1, Some("track")).expect("liste")[0]
        .service_id
        .clone();
    assert_eq!(
        favoris
            .reference_d_album(1, "track", "bandcamp", &rendu)
            .unwrap()
            .as_deref(),
        Some(PAGE),
        "le favori a pris la page que la file connaissait"
    );
    // Seul le favori sait encore d'où vient la piste.
    b.file().clear(ecoute).expect("vider la file");

    let zone = b.zone("Parents");
    b.file()
        .append(zone, &[b.entree(&rendu, None)])
        .expect("le favori remis en file, sans page, comme le client l'envoie");
    let (statut, octets) = b.jouer_la_file(zone).await;

    assert!(
        matches!(statut, 200 | 206),
        "le favori est resigné par sa page (statut {statut})"
    );
    assert_eq!(octets, b.corps);
    assert_eq!(b.relectures(), vec![PAGE.to_string()]);
}

/// L'historique : une écoute passée garde la page, et c'est elle qui permet de
/// resigner la même piste rejouée sans page.
#[tokio::test]
async fn une_ecoute_de_l_historique_rejouee_est_resignee() {
    let b = banc().await;
    HistoryRepo::with_backend(b.db.clone())
        .record(&ListenRecord {
            title: "Meet Her At The Love Parade (Framewerk Rewerk)".into(),
            source: "bandcamp".into(),
            source_id: Some(b.perimee.clone()),
            duration_ms: 300_000,
            album_ref: Some(PAGE.into()),
            ..ListenRecord::default()
        })
        .expect("historique");
    let zone = b.zone("Parents");
    b.file()
        .append(zone, &[b.entree(&b.perimee, None)])
        .expect("file");

    let (statut, octets) = b.jouer_la_file(zone).await;

    assert!(
        matches!(statut, 200 | 206),
        "la page gardée par l'historique resigne la piste (statut {statut})"
    );
    assert_eq!(octets, b.corps);
    assert_eq!(b.relectures(), vec![PAGE.to_string()]);
}

/// Contre-épreuve : une entrée antérieure à la migration 114, que rien ne
/// relie à une page, échoue comme avant — 502, et aucune page relue.
#[tokio::test]
async fn sans_page_connue_le_relais_echoue_comme_avant() {
    let b = banc().await;
    let zone = b.zone("Parents");
    b.file()
        .append(zone, &[b.entree(&b.perimee, None)])
        .expect("file");

    let (statut, _) = b.jouer_la_file(zone).await;

    assert_eq!(
        statut, 502,
        "sans page, pas de nouvelle résolution : le relais rend 502 comme avant"
    );
    assert!(b.relectures().is_empty(), "aucune page à relire");
}
