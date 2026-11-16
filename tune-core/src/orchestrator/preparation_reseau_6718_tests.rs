//! #6718 — la suivante Qobuz/Tidal d'une zone OpenHome à traitement actif se
//! prépare PENDANT la courante, et l'armement la reprend au lieu de tout
//! recommencer (le LUXMAN NT-07 du fil 2363 : 6 s de pré-transcodage complet
//! par piste, au-delà du budget de 10 s de #5970 sur une piste plus longue).
//!
//! Orchestrateur réel, vraie `OpenHomeOutput` contre un faux renderer qui
//! n'annonce que le FLAC, égaliseur armé, service Tidal simulé dont la porte
//! remplace le délai réseau. Le fichier est la référence FLAC 24/96.
use crate::TuneError;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;
use crate::streaming::traits::*;
use axum::{Router, routing::post};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

const ID: &str = "uuid:luxman-nt07-6718-prep";
const COURANTE: &str = "tidal-courante-6718";
const SUIVANTE: &str = "tidal-suivante-6718";

struct TidalLent {
    url: String,
    appels: Arc<AtomicUsize>,
    entree: Arc<Notify>,
    liberation: Arc<Notify>,
}

#[async_trait::async_trait]
impl StreamingService for TidalLent {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
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
        Err("la ligne de file porte les métadonnées".into())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        self.entree.notify_one();
        self.liberation.notified().await;
        Ok(StreamUrl {
            url: self.url.clone(),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "flac".into(),
                sample_rate: 96_000,
                bit_depth: 24,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: vec![],
        })
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

/// Un faux renderer OpenHome qui n'annonce que le FLAC : pas de WAV progressif.
async fn renderer_openhome() -> u16 {
    let app = Router::new().route(
        "/Playlist",
        post(|| async {
            "<s:Envelope><s:Body><u:ProtocolInfoResponse><Value>http-get:*:audio/flac:*\
             </Value></u:ProtocolInfoResponse></s:Body></s:Envelope>"
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    port
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    appels: Arc<AtomicUsize>,
    entree: Arc<Notify>,
    liberation: Arc<Notify>,
    _fichier: tempfile::NamedTempFile,
}

/// La zone du fil 2363 : OpenHome, enchaînement actif, deux pistes Tidal en
/// file, la première en lecture. `egaliseur` : un égaliseur armé ou non.
async fn banc(egaliseur: bool) -> Banc {
    let port = renderer_openhome().await;
    let mut services = HashMap::new();
    services.insert("playlist".to_string(), "/Playlist".to_string());
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(crate::outputs::openhome::OpenHomeOutput::new(
        "LUXMAN NT-07".into(),
        ID.into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )));
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(registre)),
        None,
    ));
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("LUXMAN", Some("openhome"), Some(ID))
        .unwrap();
    if egaliseur {
        let settings = SettingsRepo::with_backend(db.clone());
        let profil = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 1000.0,
                gain: 3.0,
                q: 0.71,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        settings
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&profil).unwrap(),
            )
            .unwrap();
        settings.set("plugin_equalizer_installed", "true").unwrap();
    }

    let fichier = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/flac/ref_24_96000_stereo.flac"
        ),
        fichier.path(),
    )
    .unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    let entree = Arc::new(Notify::new());
    let liberation = Arc::new(Notify::new());
    orch.services.lock().await.register(Box::new(TidalLent {
        url: format!("file://{}", fichier.path().display()),
        appels: appels.clone(),
        entree: entree.clone(),
        liberation: liberation.clone(),
    }));
    PlayQueueRepo::with_backend(db.clone())
        .set_streaming_queue(
            zone_id,
            &[
                (
                    "courante".into(),
                    COURANTE.into(),
                    "artiste".into(),
                    None,
                    None,
                    600_000,
                    Some("tidal".into()),
                    None,
                    None,
                ),
                (
                    "suivante".into(),
                    SUIVANTE.into(),
                    "artiste".into(),
                    None,
                    None,
                    600_000,
                    Some("tidal".into()),
                    None,
                    None,
                ),
            ],
        )
        .unwrap();
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "courante".into(),
                source: "tidal".into(),
                source_id: Some(COURANTE.into()),
                duration_ms: 600_000,
                ..Default::default()
            },
        )
        .await;
    playback.update_queue_info(zone_id, 0, 2).await;
    Banc {
        orch,
        playback,
        zone_id,
        appels,
        entree,
        liberation,
        _fichier: fichier,
    }
}

async fn maintenir(banc: &Banc) {
    let etat = banc.playback.get_state(banc.zone_id).await;
    banc.orch.maintenir_preparation_fichier(&etat).await;
}

/// Les fichiers produits par les sessions du banc, rendus jetables.
async fn fichiers_du_banc(banc: &Banc) -> Vec<tempfile::TempPath> {
    let registre = banc.orch.streamer.sessions_state();
    let sessions: Vec<_> = registre.lock().await.values().cloned().collect();
    let mut fichiers = Vec::new();
    for session in sessions {
        if let Some(path) = session.file_path.lock().await.clone()
            && std::path::Path::new(&path).is_absolute()
            && std::path::Path::new(&path).exists()
            && let Ok(p) = tempfile::TempPath::try_from_path(path)
        {
            fichiers.push(p);
        }
    }
    fichiers
}

/// 🔴 Le témoin de #6718. Rouge avant : seule une zone HQPlayer préparait sa
/// suivante d'avance ; vers OpenHome, rien ne partait avant la fenêtre des
/// trente dernières secondes, et l'armement refaisait tout le pré-transcodage
/// dans son budget de 10 s.
#[tokio::test(flavor = "multi_thread")]
async fn une_suivante_traitee_se_prepare_et_se_reprend_sur_openhome_6718() {
    let banc = banc(true).await;
    maintenir(&banc).await;
    tokio::time::timeout(Duration::from_secs(2), banc.entree.notified())
        .await
        .expect(
            "la préparation de la suivante doit commencer pendant la courante \
             sur une zone OpenHome à traitement actif (#6718)",
        );
    assert_eq!(banc.appels.load(Ordering::SeqCst), 1);
    assert!(banc.orch.fichier_suivant_en_preparation(banc.zone_id));
    assert!(
        !banc.orch.preparation_fichier_arme_tot(banc.zone_id),
        "un renderer réseau garde sa fenêtre d'armement : seul HQPlayer arme tôt"
    );
    assert!(
        banc.orch.preparation_fichier_indisponible(banc.zone_id),
        "tant que le fichier n'est pas prêt, l'armement attend sans budget"
    );

    // Une seule piste d'avance : un second passage ne relance rien.
    maintenir(&banc).await;
    assert_eq!(banc.appels.load(Ordering::SeqCst), 1);

    let fin = banc.orch.fin_preparation_fichier(banc.zone_id);
    banc.liberation.notify_one();
    let mut fin = fin;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !*fin.borrow() {
            fin.changed().await.expect("fin de la tâche signalée");
        }
    })
    .await
    .expect("la préparation doit se terminer");
    assert!(!banc.orch.preparation_fichier_indisponible(banc.zone_id));

    // L'armement (le chemin de `SetNext`) reprend le fichier préparé.
    let resolu = tokio::time::timeout(
        Duration::from_secs(5),
        banc.orch.resolve_queue_item_url(banc.zone_id, 1),
    )
    .await
    .expect("le fichier préparé doit être repris tout de suite")
    .expect("résolution de la suivante");
    let _jetables = fichiers_du_banc(&banc).await;
    assert!(resolu.stream_id.is_some());
    assert_eq!(
        banc.appels.load(Ordering::SeqCst),
        1,
        "la suivante ne doit pas être redemandée ni pré-transcodée une seconde fois"
    );
}

/// CONTRE-ÉPREUVE PERMANENTE : la même zone, sans traitement. Le flux part
/// en relais direct, rien n'est préparé d'avance.
#[tokio::test(flavor = "multi_thread")]
async fn sans_traitement_rien_ne_se_prepare_d_avance_6718() {
    let banc = banc(false).await;
    maintenir(&banc).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), banc.entree.notified())
            .await
            .is_err(),
        "sans traitement, la suivante n'a pas à être préparée d'avance"
    );
    assert_eq!(banc.appels.load(Ordering::SeqCst), 0);
    assert!(!banc.orch.fichier_suivant_en_preparation(banc.zone_id));
}

/// La file change : la préparation est annulée, la nouvelle suivante prend sa
/// place (une seule piste d'avance).
#[tokio::test(flavor = "multi_thread")]
async fn une_file_videe_annule_la_preparation_6718() {
    let banc = banc(true).await;
    maintenir(&banc).await;
    tokio::time::timeout(Duration::from_secs(2), banc.entree.notified())
        .await
        .expect("préparation lancée");
    // Plus de suivante : la file ne garde que la courante.
    PlayQueueRepo::with_backend(banc.orch.db.clone())
        .set_streaming_queue(
            banc.zone_id,
            &[(
                "courante".into(),
                COURANTE.into(),
                "artiste".into(),
                None,
                None,
                600_000,
                Some("tidal".into()),
                None,
                None,
            )],
        )
        .unwrap();
    banc.playback.update_queue_info(banc.zone_id, 0, 1).await;
    maintenir(&banc).await;
    assert!(
        !banc.orch.fichier_suivant_en_preparation(banc.zone_id),
        "sans suivante, la préparation doit être annulée"
    );
    banc.liberation.notify_one();
}
