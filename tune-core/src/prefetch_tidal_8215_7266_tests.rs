//! #8215 / #7266 — le préchargement Tidal ne fait plus attendre la lecture.
//!
//! Pour Tidal en DASH, `get_track_url` télécharge la piste suivante ENTIÈRE
//! sous le verrou en écriture du service. Le préchargement le demandait en
//! qualité maximale (`None`, donc HI_RES_LOSSLESS) quelle que soit la qualité
//! de la zone. Et ni la lecture d'un autre album ni l'arrêt ne
//! l'interrompaient : la lecture suivante attendait ce téléchargement
//! (56,8 s mesurées par Calum au lieu d'une dizaine ; « ouvrir un album prend
//! longtemps » chez Milosz).
//!
//! Le faux service note la qualité demandée. Pour la piste « LENTE », il ne
//! rend jamais la main, comme un téléchargement DASH sans fin.
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::prefetch::PrefetchEngine;
use crate::streaming::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

type Demandes = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

struct FauxTidal {
    demandes: Demandes,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for FauxTidal {
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
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        self.demandes
            .lock()
            .unwrap()
            .push((track_id.to_string(), quality.map(str::to_string)));
        if track_id == "LENTE" {
            // Un téléchargement DASH qui n'en finit pas.
            std::future::pending::<()>().await;
        }
        Err(non_servi())
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

fn titre_tidal(source_id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "tidal".into(),
        source_id: source_id.into(),
        title: format!("Titre {source_id}"),
        artist: "Quelqu'un".into(),
        album: Some("Un album".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

struct Banc {
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    playback: Arc<PlaybackManager>,
    services: Arc<Mutex<ServiceRegistry>>,
    demandes: Demandes,
}

/// Une zone qui joue `A` ; la file est `[A, suivante]`.
async fn banc(suivante: &str, qualite: Option<&str>) -> Banc {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("dlna"), Some("uuid:banc-8215"))
        .unwrap();
    if let Some(q) = qualite {
        SettingsRepo::with_backend(db.clone())
            .set(
                &format!("zone_{zone_id}_quality"),
                &serde_json::json!({ "quality": q }).to_string(),
            )
            .unwrap();
    }
    PlayQueueRepo::with_backend(db.clone())
        .append(zone_id, &[titre_tidal("A"), titre_tidal(suivante)])
        .unwrap();
    let playback = Arc::new(PlaybackManager::new());
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "Titre A".into(),
                source: "tidal".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    playback.update_queue_info(zone_id, 0, 2).await;
    let demandes: Demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(FauxTidal {
        demandes: demandes.clone(),
    }));
    Banc {
        db,
        zone_id,
        playback,
        services: Arc::new(Mutex::new(registre)),
        demandes,
    }
}

/// Lance le préchargement en tâche de fond et attend qu'il soit chez le
/// service.
async fn prechargement_en_cours(
    banc: &Banc,
    moteur: &Arc<PrefetchEngine>,
) -> tokio::task::JoinHandle<()> {
    let tache = {
        let moteur = moteur.clone();
        let (db, services, playback, zone) = (
            banc.db.clone(),
            banc.services.clone(),
            banc.playback.clone(),
            banc.zone_id,
        );
        tokio::spawn(async move { moteur.prefetch_next(db, services, playback, zone).await })
    };
    for _ in 0..200 {
        if !banc.demandes.lock().unwrap().is_empty() {
            return tache;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("le préchargement n'a jamais interrogé le service");
}

/// Le verrou du service est-il repris dans la seconde ?
async fn service_libre(banc: &Banc) -> bool {
    tokio::time::timeout(Duration::from_secs(1), async {
        let svc = banc.services.lock().await.get("tidal").expect("faux Tidal");
        drop(svc.write().await);
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn une_zone_en_qualite_cd_precharge_en_qualite_cd_7266() {
    let banc = banc("B", Some("cd")).await;
    PrefetchEngine::new()
        .prefetch_next(
            banc.db.clone(),
            banc.services.clone(),
            banc.playback.clone(),
            banc.zone_id,
        )
        .await;
    assert_eq!(
        *banc.demandes.lock().unwrap(),
        vec![("B".to_string(), Some("LOSSLESS".to_string()))],
        "une zone en qualité CD doit précharger en LOSSLESS, pas en HI_RES_LOSSLESS \
         (`None` = maximum du service) (#7266)"
    );
}

#[tokio::test]
async fn une_zone_en_qualite_max_garde_le_maximum_du_service_7266() {
    let banc = banc("B", None).await;
    PrefetchEngine::new()
        .prefetch_next(
            banc.db.clone(),
            banc.services.clone(),
            banc.playback.clone(),
            banc.zone_id,
        )
        .await;
    assert_eq!(
        *banc.demandes.lock().unwrap(),
        vec![("B".to_string(), None)]
    );
}

#[tokio::test]
async fn la_lecture_d_un_autre_album_n_attend_plus_le_prechargement_8215() {
    let banc = banc("LENTE", None).await;
    let moteur = Arc::new(PrefetchEngine::new());
    let tache = prechargement_en_cours(&banc, &moteur).await;
    assert!(
        !service_libre(&banc).await,
        "prémisse : le préchargement tient le service"
    );

    assert!(moteur.abandonner_pour_une_autre_piste("tidal", "AUTRE-ALBUM"));
    assert!(
        service_libre(&banc).await,
        "la lecture d'une autre piste doit reprendre le service tout de suite, \
         pas attendre la fin du téléchargement préchargé (#8215)"
    );
    tokio::time::timeout(Duration::from_secs(2), tache)
        .await
        .expect("le préchargement abandonné se termine")
        .unwrap();
}

#[tokio::test]
async fn la_piste_prechargee_elle_meme_n_est_pas_abandonnee_8215() {
    let banc = banc("LENTE", None).await;
    let moteur = Arc::new(PrefetchEngine::new());
    let tache = prechargement_en_cours(&banc, &moteur).await;
    assert!(
        !moteur.abandonner_pour_une_autre_piste("tidal", "LENTE"),
        "la lecture de la piste préchargée reprend son téléchargement (#7247)"
    );
    assert!(!service_libre(&banc).await);
    tache.abort();
}

#[tokio::test]
async fn l_arret_abandonne_le_prechargement_en_cours_7266() {
    let banc = banc("LENTE", None).await;
    let moteur = Arc::new(PrefetchEngine::new());
    let tache = prechargement_en_cours(&banc, &moteur).await;
    moteur.clear().await;
    assert!(
        service_libre(&banc).await,
        "après l'arrêt, la lecture suivante ne doit pas attendre le préchargement (#7266)"
    );
    tokio::time::timeout(Duration::from_secs(2), tache)
        .await
        .expect("le préchargement abandonné se termine")
        .unwrap();
}
