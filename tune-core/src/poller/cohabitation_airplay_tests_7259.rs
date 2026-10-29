use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Sortie {
    id: &'static str,
    type_sortie: &'static str,
    host: Option<&'static str>,
    etat: TransportState,
    appels: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl OutputTarget for Sortie {
    fn name(&self) -> &str {
        self.id
    }
    fn device_id(&self) -> &str {
        self.id
    }
    fn output_type(&self) -> &str {
        self.type_sortie
    }
    fn host(&self) -> Option<&str> {
        self.host
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn is_available(&self) -> bool {
        true
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        Ok(OutputStatus {
            state: self.etat,
            position_ms: 1000,
            duration_ms: 240_000,
            ..Default::default()
        })
    }
}

async fn sonder(
    type_actif: &'static str,
    hote_actif: Option<&'static str>,
    en_pause: bool,
    arreter: bool,
) -> usize {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(db.clone());
    let repos = repo
        .create("Marantz DLNA", Some("dlna"), Some("dlna:marantz"))
        .unwrap();
    let actif = repo
        .create("DENON,1", Some(type_actif), Some("actif:marantz"))
        .unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(Sortie {
        id: "dlna:marantz",
        type_sortie: "dlna",
        host: Some("127.0.0.1"),
        etat: TransportState::Stopped,
        appels: appels.clone(),
    }));
    outputs.lock().await.register(Box::new(Sortie {
        id: "actif:marantz",
        type_sortie: type_actif,
        host: hote_actif,
        etat: if en_pause {
            TransportState::Paused
        } else {
            TransportState::Playing
        },
        appels: Arc::new(AtomicUsize::new(0)),
    }));
    let playback = Arc::new(PlaybackManager::new());
    playback
        .play(
            actif,
            crate::playback::NowPlaying {
                title: "PCM factice".into(),
                duration_ms: 240_000,
                ..Default::default()
            },
        )
        .await;
    if en_pause {
        playback.pause(actif).await;
    }
    if arreter {
        playback.stop(actif).await;
    }
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db,
        Arc::new(Mutex::new(HashMap::new())),
    );
    poller
        .tick(&mut HashMap::new(), &mut HashMap::new(), &Instant::now())
        .await;
    assert_eq!(
        playback.get_state(repos).await.state,
        PlayState::Stopped,
        "la cohabitation ne doit pas démarrer le DLNA"
    );
    appels.load(Ordering::SeqCst)
}

#[tokio::test]
async fn dlna_au_repos_n_interroge_pas_le_recepteur_en_airplay_7259() {
    assert_eq!(
        sonder("airplay", Some("127.0.0.1"), false, false).await,
        0,
        "une interrogation DLNA perturbe encore la session AirPlay du même hôte"
    );
}
#[tokio::test]
async fn session_airplay_en_pause_reste_protegee_7259() {
    assert_eq!(
        sonder("airplay", Some("127.0.0.1"), true, false).await,
        0,
        "le DLNA interroge encore le récepteur pendant sa session AirPlay en pause"
    );
}
#[tokio::test]
async fn dlna_reprend_les_sondages_apres_arret_airplay_7259() {
    assert_eq!(sonder("airplay", Some("127.0.0.1"), false, true).await, 1);
}
#[tokio::test]
async fn les_autres_hotes_et_protocoles_restent_sondes_7259() {
    assert_eq!(sonder("airplay", Some("127.0.0.2"), false, false).await, 1);
    assert_eq!(sonder("airplay", None, false, false).await, 1);
    assert_eq!(sonder("oaat", Some("127.0.0.1"), false, false).await, 1);
}
