use super::*;
use crate::db::zone_repo::ZoneRepo;
use std::sync::atomic::{AtomicU32, Ordering};

/// Renderer poli : il répond toujours, et il répond ce qu'on lui dit de
/// répondre. Il compte les `get_status` reçus — c'est exactement le
/// trafic SOAP que #2263 mesure.
///
/// Son état est PARTAGÉ et modifiable en cours de banc : c'est la seule
/// façon de mesurer ce que coûte une pause, puis ce qu'elle rapporte quand
/// l'appareil en ressort — sans qu'aucune horloge n'entre dans le test.
struct Compteur {
    etat: Arc<std::sync::Mutex<TransportState>>,
    appels: Arc<AtomicU32>,
}
#[async_trait::async_trait]
impl OutputTarget for Compteur {
    fn name(&self) -> &str {
        "compteur"
    }
    fn device_id(&self) -> &str {
        "test:compteur"
    }
    fn output_type(&self) -> &str {
        "test"
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
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        self.appels.fetch_add(1, Ordering::Relaxed);
        let state = *self.etat.lock().expect("état du renderer bouchonné");
        Ok(OutputStatus {
            state,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Le banc : la VRAIE boucle `tick()`, câblée à un renderer qui compte les
/// sondages qu'il subit.
///
/// Aucune horloge n'y intervient — on y compte des TOURS, jamais des
/// secondes. Un test de cadence qui dormirait pour de bon serait
/// intermittent, et il empoisonnerait la suite entière.
struct Banc {
    poller: PositionPoller,
    appels: Arc<AtomicU32>,
    etat: Arc<std::sync::Mutex<TransportState>>,
    poll_states: HashMap<i64, ZonePollState>,
    idle_backoff: HashMap<i64, IdlePollBackoff>,
    startup_at: Instant,
}

impl Banc {
    async fn neuf(etat: TransportState) -> Self {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let device_id = "test:compteur";
        ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("test"), Some(device_id))
            .unwrap();
        let appels = Arc::new(AtomicU32::new(0));
        let etat = Arc::new(std::sync::Mutex::new(etat));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(Compteur {
            etat: etat.clone(),
            appels: appels.clone(),
        }));
        let playback = Arc::new(crate::playback::PlaybackManager::new());
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
            playback,
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            appels,
            etat,
            poll_states: HashMap::new(),
            idle_backoff: HashMap::new(),
            startup_at: Instant::now(),
        }
    }

    /// Ce que le renderer répondra à partir du prochain sondage.
    fn poser_etat(&self, etat: TransportState) {
        *self.etat.lock().expect("état du renderer bouchonné") = etat;
    }

    /// Joue `ticks` tours et rend le nombre de sondages subis PENDANT
    /// ceux-là : le compteur repart de zéro à l'entrée.
    async fn jouer(&mut self, ticks: u32) -> u32 {
        self.appels.store(0, Ordering::Relaxed);
        for _ in 0..ticks {
            self.poller
                .tick(
                    &mut self.poll_states,
                    &mut self.idle_backoff,
                    &self.startup_at,
                )
                .await;
        }
        self.appels.load(Ordering::Relaxed)
    }
}

/// Joue `ticks` tours de la vraie boucle `tick()` sur une zone que Tune ne
/// croit pas en lecture, câblée à un renderer qui répond `etat`, et rend le
/// nombre de sondages qu'il a réellement subis.
async fn sondages_sur(etat: TransportState, ticks: u32) -> u32 {
    Banc::neuf(etat).await.jouer(ticks).await
}

/// Le défaut de #2263, mesuré sur la vraie boucle : une zone arrêtée dont
/// le renderer répond poliment était sondée à CHAQUE tick, indéfiniment.
///
/// Ce test couvre le site d'appel, pas seulement `IdlePollBackoff` :
/// neutraliser l'état passé à `record_success` laissait les tests
/// unitaires du recul entièrement verts.
#[tokio::test]
async fn une_zone_arretee_est_sondee_a_la_cadence_de_repos() {
    let sondages = sondages_sur(TransportState::Stopped, 10).await;
    assert_eq!(
        sondages,
        10u32.div_ceil(IDLE_REPOS_POLL_TICKS as u32),
        "10 ticks sur une zone arrêtée doivent tenir en {} sondages, pas {sondages}",
        10u32.div_ceil(IDLE_REPOS_POLL_TICKS as u32)
    );
}

/// Le frein ne doit jamais toucher un appareil qui joue : la reprise
/// d'état après une lecture lancée depuis la façade, la synchronisation du
/// volume et la détection de conflit gardent leur cadence d'aujourd'hui.
#[tokio::test]
async fn un_renderer_qui_joue_reste_sonde_a_chaque_tick() {
    assert_eq!(sondages_sur(TransportState::Playing, 10).await, 10);
}

/// LA MESURE de cette passe : une zone laissée EN PAUSE.
///
/// Elle n'est pas `PlayState::Playing`, donc elle passe par la branche
/// « repos » ; et son renderer répond `PAUSED_PLAYBACK`, que le recul
/// comptait comme un transport actif — plein rythme, indéfiniment.
/// Soixante tours de pause coûtaient donc 60 sondages, soit 180 actions
/// SOAP par minute (`GetPositionInfo` + `GetTransportInfo` + `GetVolume`)
/// pour n'apprendre rien : la branche ne fait RIEN d'un statut en pause,
/// ses deux seuls consommateurs exigeant `Playing` (#2263).
#[tokio::test]
async fn une_zone_en_pause_est_sondee_a_la_cadence_de_repos() {
    let sondages = sondages_sur(TransportState::Paused, 60).await;
    let attendu = 60u32.div_ceil(IDLE_REPOS_POLL_TICKS as u32);
    assert_eq!(
        sondages, attendu,
        "60 tours en pause doivent tenir en {attendu} sondages, pas {sondages}"
    );
    assert!(
        sondages < 60,
        "60 sondages par minute étaient le défaut corrigé, or {sondages}"
    );
}

/// TÉMOIN, vert des deux côtés : le frein ne troque pas un gaspillage
/// contre une cécité.
///
/// Un doigt sur la façade de l'appareil sort la zone de sa pause. Cette
/// reprise doit être VUE, dans le délai ANNONCÉ — au plus
/// [`IDLE_REPOS_POLL_TICKS`] tours — et la cadence repartir aussitôt à
/// plein régime. Sans ce cas, ralentir la pause au point de ne plus
/// jamais la resonder passerait aussi.
#[tokio::test]
async fn une_pause_qui_repart_est_vue_puis_rend_le_plein_rythme() {
    let mut banc = Banc::neuf(TransportState::Paused).await;
    banc.jouer(20).await;
    banc.poser_etat(TransportState::Playing);
    let vus = banc.jouer(IDLE_REPOS_POLL_TICKS as u32).await;
    assert!(
        vus >= 1,
        "la reprise doit être vue en au plus {IDLE_REPOS_POLL_TICKS} tours, or 0 sondage"
    );
    assert_eq!(
        banc.jouer(10).await,
        10,
        "une fois la lecture vue, plus aucun frein"
    );
}
