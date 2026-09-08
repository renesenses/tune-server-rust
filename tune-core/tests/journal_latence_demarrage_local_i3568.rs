//! #3568 — le démarrage d'une piste de service sur SORTIE LOCALE laisse enfin
//! trois durées dans le journal, au niveau livré.
//!
//! # Ce que le silence a coûté
//!
//! FranckLeRouge, le 07/09/2026 sur forum-hifi 41831 #628, depuis un PC
//! Windows : « la lecture depuis Tidal est immédiate via Audirvana ». Avec Tune
//! sur sa sortie locale, elle ne l'est pas. C'est son TROISIÈME signalement de
//! latence de démarrage (#2350, #2352) et il n'existe toujours **aucune
//! mesure** — ni de sa part, ni dans nos journaux.
//!
//! L'écart est attendu par construction, et le code le dit : `LocalOutput` ne
//! décode pas de flux compressé, donc `resoudre_flux_local_ou_oaat` télécharge
//! le flux ENTIER vers un fichier temporaire, puis le décode en PCM WAV.
//! Audirvana, lui, lit l'adresse Tidal en progressif. Ce que personne ne peut
//! dire, c'est **combien**, et **quelle étape domine** — assemblage du fMP4
//! DASH, téléchargement HTTPS, ou transcodage.
//!
//! Le journal ne permettait pas de trancher : `get_track_url()` — l'appel qui
//! assemble le fMP4 DASH sur disque — n'était pas chronométré du tout, et les
//! deux seules traces des étapes suivantes étaient des `debug!`, donc absentes
//! du niveau livré et de l'export de diagnostic qu'un testeur joint à son fil.
//!
//! # Ce que ce témoin fixe
//!
//! Une résolution de piste de service sur une zone dont `output_device_id`
//! commence par `local:` émet, au niveau INFO :
//!
//!   1. `streaming_track_url_resolved` avec `elapsed_ms` et `origine` ;
//!   2. `streaming_download_complete` avec `elapsed_ms` et `octets` ;
//!   3. `streaming_transcode_complete_progressive` avec `elapsed_ms`.
//!
//! Ces trois lignes, datées, donnent la décomposition que le ticket réclame.
//! Ce témoin ne prétend PAS que le délai a diminué : il ne mesure rien de la
//! latence réelle, il tient le fait que la mesure existe.
//!
//! # Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665, #2890, #3180 et #3479, déjà payée quatre fois :
//! `tracing` met en cache POUR TOUT LE PROCESSUS la décision « ce point d'appel
//! intéresse-t-il quelqu'un ? ». Un abonné posé au milieu d'une suite qui
//! tourne en parallèle se voit priver d'évènements de façon imprévisible. Ici
//! l'abonné est GLOBAL et ce fichier ne contient QU'UN test.
//! `autotests = false` dans `tune-core/Cargo.toml` — la cible y est déclarée,
//! sans quoi ce fichier ne serait jamais compilé, et la porte rendrait un vert
//! contre rien.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as MutexAsync;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

const SERVICE: &str = "factice";
const PISTE: &str = "piste-1";
const MIME: &str = "audio/wav";

/// Ce que le service factice a vu passer.
#[derive(Default)]
struct Journal {
    get_track_url: AtomicUsize,
    get_track: AtomicUsize,
    refresh: AtomicUsize,
}

struct ServiceFactice {
    journal: Arc<Journal>,
    /// L'URL que le service « signe » pour la piste.
    url: String,
    /// Nombre d'appels à `get_track_url` qui échouent en 401 avant de réussir.
    echecs_401: AtomicUsize,
    /// Ce que `get_track` rend quand l'orchestrateur vient chercher les
    /// métadonnées.
    piste: StreamTrack,
}

impl ServiceFactice {
    fn nouveau(journal: Arc<Journal>, url: &str, echecs_401: usize) -> Self {
        Self {
            journal,
            url: url.into(),
            echecs_401: AtomicUsize::new(echecs_401),
            piste: StreamTrack {
                id: PISTE.into(),
                title: "Titre du service".into(),
                artist: "Artiste du service".into(),
                album: Some("Album du service".into()),
                album_id: None,
                artist_id: None,
                composer: None,
                isrc: None,
                duration_ms: 241_000,
                cover_path: None,
                track_number: Some(3),
                disc_number: Some(1),
                explicit: false,
                quality: None,
            },
        }
    }

    fn qualite() -> StreamQuality {
        StreamQuality {
            codec: "WAV".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            bitrate: None,
            channels: 2,
        }
    }
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le service factice ne sert pas : {quoi}"))
}

#[async_trait::async_trait]
impl StreamingService for ServiceFactice {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        SERVICE
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

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        self.journal.get_track.fetch_add(1, Ordering::SeqCst);
        if track_id == PISTE {
            Ok(self.piste.clone())
        } else {
            Err(TuneError::NotFound(format!("piste inconnue : {track_id}")))
        }
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        self.journal.get_track_url.fetch_add(1, Ordering::SeqCst);
        if track_id != PISTE {
            return Err(TuneError::NotFound(format!("piste inconnue : {track_id}")));
        }
        // Simule un jeton périmé : les N premiers appels tombent en 401.
        if self
            .echecs_401
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(TuneError::Streaming("HTTP 401 Unauthorized".into()));
        }
        Ok(StreamUrl {
            url: self.url.clone(),
            mime_type: MIME.into(),
            quality: Self::qualite(),
            expires_at: None,
        })
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        self.journal.refresh.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_album_tracks"))
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

// ---------------------------------------------------------------------------
// Le journal, et le CDN simulé
// ---------------------------------------------------------------------------

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois que FranckLeRouge en joindra un.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn lire(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

/// Un WAV PCM 16 bits, 44,1 kHz, stéréo, ~0,2 s de silence.
///
/// Un vrai conteneur, pas une chaîne d'octets : c'est symphonia qui le décode
/// dans la tâche de transcodage, et un en-tête bricolé ferait échouer l'étape
/// dont on veut justement la durée.
fn wav_de_test() -> Vec<u8> {
    const ECHANTILLONS: u32 = 8_820; // 0,2 s à 44,1 kHz
    let octets_donnees = ECHANTILLONS * 2 * 2; // stéréo, 16 bits
    let mut v = Vec::with_capacity(44 + octets_donnees as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_donnees).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // taille du bloc fmt
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&2u16.to_le_bytes()); // canaux
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes()); // octets/s
    v.extend_from_slice(&4u16.to_le_bytes()); // alignement de bloc
    v.extend_from_slice(&16u16.to_le_bytes()); // bits par échantillon
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_donnees.to_le_bytes());
    v.resize(44 + octets_donnees as usize, 0);
    v
}

/// Monte un CDN simulé qui sert ce WAV, et rend l'adresse de la piste.
async fn cdn_simule() -> String {
    let app = axum::Router::new().route(
        "/piste.wav",
        axum::routing::get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "audio/wav")],
                wav_de_test(),
            )
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}/piste.wav")
}

/// La ligne du journal qui porte cet évènement, ou `None`.
fn ligne<'a>(journal: &'a str, evenement: &str) -> Option<&'a str> {
    journal.lines().find(|l| l.contains(evenement))
}

/// `elapsed_ms=<n>` dans une ligne, lu comme un nombre.
///
/// C'est le champ qui compte : une ligne SANS durée ne mesure rien, et c'est
/// exactement ce qu'était `streaming_transcode_complete_progressive` avant
/// #3568 — un `debug!` sans un seul chiffre.
fn duree_ms(ligne: &str) -> u64 {
    let apres = ligne
        .split("elapsed_ms=")
        .nth(1)
        .unwrap_or_else(|| panic!("aucun elapsed_ms dans : {ligne}"));
    let chiffres: String = apres.chars().take_while(char::is_ascii_digit).collect();
    chiffres
        .parse()
        .unwrap_or_else(|_| panic!("elapsed_ms illisible dans : {ligne}"))
}

// ---------------------------------------------------------------------------
// Le témoin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn le_demarrage_sur_sortie_locale_dit_ses_trois_durees_au_niveau_livre() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let url = cdn_simule().await;

    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);

    let journal_service = Arc::new(Journal::default());
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceFactice::nouveau(
        journal_service.clone(),
        &url,
        0,
    )));

    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(MutexAsync::new(registre)),
        Arc::new(MutexAsync::new(OutputRegistry::new())),
        None,
    );

    // C'est `output_device_id` qui choisit la branche : `local:` fait entrer
    // dans `resoudre_flux_local_ou_oaat`, celle qui télécharge puis transcode.
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon (Windows)", Some("local"), Some("local:DAC"))
        .expect("zone");

    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[QueueInput::Streaming {
                source: SERVICE.into(),
                source_id: PISTE.into(),
                title: "Titre en file".into(),
                artist: "Artiste en file".into(),
                album: Some("Album en file".into()),
                cover_url: None,
                duration_ms: 180_000,
                track_number: Some(1),
                disc_number: Some(1),
            }],
        )
        .expect("file");

    let resolu = orch
        .resolve_queue_item_url(zone_id, 0)
        .await
        .expect("résolution");
    assert_eq!(
        resolu.mime_type, "audio/wav",
        "la branche locale transcode en WAV : sinon ce témoin ne tient pas le bon chemin"
    );

    // Téléchargement et transcodage tournent dans une tâche détachée : on
    // attend LEURS lignes, pas un délai arbitraire.
    let mut journal = String::new();
    for _ in 0..400 {
        journal = capture.lire();
        if journal.contains("streaming_transcode_complete_progressive") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // ── 1. L'appel qui assemble le fMP4 DASH chez Tidal ──────────────────────
    let l = ligne(&journal, "streaming_track_url_resolved")
        .unwrap_or_else(|| panic!("rien ne chronomètre get_track_url — journal :\n{journal}"));
    duree_ms(l);
    assert!(
        l.contains("origine="),
        "sans `origine`, le chiffre ne dit pas s'il couvre un assemblage DASH : {l}"
    );

    // ── 2. Le téléchargement complet, que `LocalOutput` impose ───────────────
    let l = ligne(&journal, "streaming_download_complete").unwrap_or_else(|| {
        panic!("le téléchargement ne dit rien au niveau INFO — journal :\n{journal}")
    });
    duree_ms(l);
    assert!(
        l.contains("octets="),
        "sans la taille, on ne sépare pas « réseau lent » de « fichier gros » : {l}"
    );

    // ── 3. Le transcodage en PCM WAV ─────────────────────────────────────────
    let l = ligne(&journal, "streaming_transcode_complete_progressive").unwrap_or_else(|| {
        panic!("le transcodage ne dit rien au niveau INFO — journal :\n{journal}")
    });
    duree_ms(l);

    assert_eq!(
        journal_service.get_track_url.load(Ordering::SeqCst),
        1,
        "une seule résolution : les durées ci-dessus décrivent bien UN démarrage"
    );
}
