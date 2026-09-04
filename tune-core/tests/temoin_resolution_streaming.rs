//! Témoin de `resolve_streaming_url` (#2219, famille resolve_stream).
//!
//! `resolve_streaming_url` fait 1 594 lignes dans `orchestrator.rs` et
//! **aucun test inline ne l'appelle** : c'est la seule famille du plan de
//! découpe sans témoin dans le fichier. Ce test la fixe AVANT qu'elle bouge,
//! par sa seule porte publique — `resolve_queue_item_url` — avec un service
//! de streaming factice enregistré dans le registre. Rien n'est privé ici,
//! rien ne touche à l'orchestrateur : le témoin survit à son déplacement.
//!
//! Ce qu'il fige, sur une zone RÉSEAU (ni `local:` ni `oaat:`), la seule
//! branche qui ne télécharge rien :
//!   1. une URL `http://` du service part telle quelle, sans session ;
//!   2. une URL `https://` est relayée par NOTRE proxy (les renderers DLNA ne
//!      parlent pas TLS) : session de flux ouverte, URL sur notre serveur ;
//!   3. la qualité annoncée par le service est propagée (fréquence, profondeur) ;
//!   4. un titre non vide en file évite tout appel réseau ; un titre vide va
//!      chercher les métadonnées auprès du service (le « Now Playing » vidé de
//!      DEvir) ;
//!   5. une durée nulle est complétée par le service (le sondeur refuse
//!      d'avancer sur une durée 0, #483) ;
//!   6. un 401 déclenche UN rafraîchissement de jeton et UN nouvel essai, et
//!      un 401 persistant remonte sans boucler ;
//!   7. un service absent du registre est nommé dans l'erreur.
//!
//! Hors de portée ici, parce que ces branches téléchargent le flux : la sortie
//! locale (transcodage en WAV) et l'AAC relayé vers un renderer. Elles relèvent
//! du banc REF-5 avec un puits de capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Mutex;
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
const URL_HTTP: &str = "http://cdn.exemple.test/piste-1.flac";
const URL_HTTPS: &str = "https://cdn.exemple.test/piste-1.flac";
const MIME: &str = "audio/flac";

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
            codec: "FLAC".into(),
            sample_rate: 96_000,
            bit_depth: 24,
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

/// Un orchestrateur sur base vierge, avec le service factice enregistré, et
/// une zone RÉSEAU (pas `local:` ni `oaat:`).
struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    journal: Arc<Journal>,
    file: PlayQueueRepo,
}

async fn banc(url: &str, echecs_401: usize) -> Banc {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);

    let journal = Arc::new(Journal::default());
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceFactice::nouveau(
        journal.clone(),
        url,
        echecs_401,
    )));

    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(registre)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );

    let zone_id = ZoneRepo::with_backend(db.clone())
        .create(
            "Salon (renderer factice)",
            Some("dlna"),
            Some("uuid:renderer-factice"),
        )
        .expect("zone");

    Banc {
        orch,
        zone_id,
        journal,
        file: PlayQueueRepo::with_backend(db),
    }
}

fn entree(source: &str, titre: &str, duree_ms: i64) -> QueueInput {
    QueueInput::Streaming {
        source: source.into(),
        source_id: PISTE.into(),
        title: titre.into(),
        artist: "Artiste en file".into(),
        album: Some("Album en file".into()),
        cover_url: None,
        duration_ms: duree_ms,
        track_number: Some(3),
        disc_number: Some(1),
    }
}

#[tokio::test]
async fn une_url_http_du_service_part_telle_quelle_sur_une_zone_reseau() {
    let b = banc(URL_HTTP, 0).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "Titre en file", 180_000)])
        .expect("file");

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution");

    assert_eq!(
        r.url, URL_HTTP,
        "en clair, l'URL du service part telle quelle : rien à relayer"
    );
    assert_eq!(r.mime_type, MIME, "le type MIME du service est conservé");
    assert_eq!(
        r.stream_id, None,
        "aucune session de flux n'est ouverte quand rien n'est relayé"
    );
    assert_eq!(
        r.sample_rate,
        Some(96_000),
        "la fréquence annoncée par le service est propagée"
    );
    assert_eq!(
        r.bit_depth,
        Some(24),
        "la profondeur annoncée par le service est propagée"
    );
    assert_eq!(r.channels, Some(2));
    assert_eq!(r.source.as_deref(), Some(SERVICE));
    assert_eq!(r.source_id.as_deref(), Some(PISTE));
    assert_eq!(
        r.title, "Titre en file",
        "le titre de la file gagne quand il est non vide"
    );
    assert_eq!(r.artist.as_deref(), Some("Artiste en file"));
    assert_eq!(r.duration_ms, Some(180_000));
    assert_eq!(
        b.journal.get_track.load(Ordering::SeqCst),
        0,
        "titre non vide et durée connue : AUCUN appel réseau de métadonnées"
    );
    assert_eq!(b.journal.get_track_url.load(Ordering::SeqCst), 1);
    assert_eq!(b.journal.refresh.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn une_url_https_est_relayee_par_notre_proxy_sur_une_zone_reseau() {
    let b = banc(URL_HTTPS, 0).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "Titre en file", 180_000)])
        .expect("file");

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution");

    assert_ne!(
        r.url, URL_HTTPS,
        "un renderer réseau ne parle pas TLS : l'URL signée ne lui est jamais donnée"
    );
    assert!(
        r.url.starts_with("http://") && r.url.contains("/stream/") && r.url.ends_with(".flac"),
        "le flux est relayé verbatim par notre serveur, sous son codec d'origine : {}",
        r.url
    );
    assert!(
        r.stream_id.is_some(),
        "le relais est une session de flux, qu'un gapless peut reprendre"
    );
    assert_eq!(
        r.mime_type, MIME,
        "relayé sans transcodage, le MIME reste celui du service"
    );
    assert_eq!(r.sample_rate, Some(96_000));
    assert_eq!(r.bit_depth, Some(24));
    assert_eq!(
        b.journal.get_track.load(Ordering::SeqCst),
        0,
        "le relais n'appelle pas les métadonnées quand la file les porte"
    );
}

#[tokio::test]
async fn un_titre_vide_en_file_est_recherche_aupres_du_service() {
    let b = banc(URL_HTTP, 0).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "", 180_000)])
        .expect("file");

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution");

    assert_eq!(
        r.title, "Titre du service",
        "un titre vide en file ne doit pas vider le Now Playing : on redemande au service"
    );
    assert!(
        b.journal.get_track.load(Ordering::SeqCst) >= 1,
        "le service a été interrogé pour les métadonnées"
    );
}

#[tokio::test]
async fn une_duree_nulle_est_completee_par_le_service() {
    let b = banc(URL_HTTP, 0).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "Titre en file", 0)])
        .expect("file");

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("résolution");

    assert_eq!(r.title, "Titre en file");
    assert_eq!(
        r.duration_ms,
        Some(241_000),
        "une durée 0 désarme l'avance du sondeur : elle est complétée par le service"
    );
    assert_eq!(
        b.journal.get_track.load(Ordering::SeqCst),
        1,
        "un seul appel de métadonnées, pour la durée seulement"
    );
}

#[tokio::test]
async fn un_401_declenche_un_seul_rafraichissement_puis_reessaie() {
    let b = banc(URL_HTTP, 1).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "Titre en file", 180_000)])
        .expect("file");

    let r = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .expect("le second essai, après rafraîchissement, doit réussir");

    assert_eq!(r.url, URL_HTTP);
    assert_eq!(
        b.journal.refresh.load(Ordering::SeqCst),
        1,
        "un rafraîchissement, pas deux"
    );
    assert_eq!(
        b.journal.get_track_url.load(Ordering::SeqCst),
        2,
        "l'URL est redemandée exactement une fois après le rafraîchissement"
    );
}

#[tokio::test]
async fn un_401_persistant_apres_rafraichissement_remonte_l_erreur() {
    let b = banc(URL_HTTP, 2).await;
    b.file
        .append(b.zone_id, &[entree(SERVICE, "Titre en file", 180_000)])
        .expect("file");

    let err = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .err()
        .expect("deux 401 de suite : on ne boucle pas, on remonte");

    assert!(
        err.contains("401"),
        "l'erreur nomme le refus du service : {err}"
    );
    assert_eq!(
        b.journal.refresh.load(Ordering::SeqCst),
        1,
        "un seul rafraîchissement tenté"
    );
    assert_eq!(
        b.journal.get_track_url.load(Ordering::SeqCst),
        2,
        "un seul nouvel essai"
    );
}

#[tokio::test]
async fn un_service_absent_du_registre_est_nomme_dans_l_erreur() {
    let b = banc(URL_HTTP, 0).await;
    b.file
        .append(b.zone_id, &[entree("inexistant", "Titre en file", 180_000)])
        .expect("file");

    let err = b
        .orch
        .resolve_queue_item_url(b.zone_id, 0)
        .await
        .err()
        .expect("un service inconnu ne peut pas résoudre");

    assert_eq!(err, "unknown service: inexistant");
    assert_eq!(b.journal.get_track_url.load(Ordering::SeqCst), 0);
}
