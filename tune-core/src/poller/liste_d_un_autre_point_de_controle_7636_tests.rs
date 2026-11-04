//! #7636 — la liste qu'un autre point de contrôle (Kazoo) a chargée dans un
//! renderer OpenHome reste intacte.
//!
//! Banc : le VRAI sondeur (`tick`), un vrai orchestrateur, une file d'une
//! piste et une sortie factice de type `openhome`. Chaque `play_media` d'une
//! sortie OpenHome commence par `DeleteAll` : compter les lectures envoyées à
//! la sortie, c'est compter les fois où la liste de l'appareil aurait été
//! vidée.
//!
//! Témoins :
//!  1. Kazoo prend la main (URI servie par le serveur média, pas un flux
//!     Tune) : après trois relevés, la zone Tune lâche l'appareil, sans lui
//!     envoyer ni `Stop` ni lecture, et ne le reprend pas ensuite, même
//!     quand Kazoo passe d'une piste à l'autre ;
//!  2. notre propre flux, ou la suivante armée d'un enchaînement (un flux
//!     Tune d'un autre identifiant), ne fait rien lâcher ;
//!  3. la même URI étrangère sur une zone DLNA ne change pas de comportement.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::outputs::traits::TransportState;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const TITRE: &str = "Spiegel im Spiegel";
const DUREE_MS: i64 = 605_000;
const NOTRE_FLUX: &str = "sid-7636";
const URI_DE_TUNE: &str = "http://192.168.1.20:8888/stream/sid-7636.flac";
/// La suivante armée d'un enchaînement : un flux Tune, d'un autre identifiant.
const URI_DE_LA_SUIVANTE: &str = "http://192.168.1.20:8888/stream/sid-suivante.flac";
/// Ce que le Linn rapporte quand Kazoo joue sa liste, tirée du serveur média
/// de Tune.
const URI_DE_KAZOO: &str = "http://192.168.1.20:8888/api/v1/library/tracks/42/audio";

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    appareil: String,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter(type_sortie: &str, appareil: &str) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Klimax", Some(type_sortie), Some(appareil))
            .unwrap();
        let mut t = crate::db::models::Track::new(TITRE.to_string());
        t.file_path = Some("/Musique/spiegel.flac".into());
        t.format = Some("flac".into());
        t.duration_ms = DUREE_MS;
        let piste = TrackRepo::with_backend(db.clone()).create(&t).unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &[piste])
            .unwrap();
        ZoneRepo::with_backend(db.clone())
            .save_play_state(zone_id, "playing")
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(appareil, "Klimax DSM").with_type(type_sortie),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(piste),
                    title: TITRE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_MS,
                    stream_id: Some(NOTRE_FLUX.into()),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, 1).await;

        Self {
            poller,
            playback,
            outputs,
            db,
            appareil: appareil.to_string(),
            zone_id,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
        }
    }

    /// Ce que la sortie rapporte au prochain sondage.
    async fn sortie(&self, etat: TransportState, position_ms: u64, uri: &str) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(DUREE_MS as u64);
        mock.set_position(position_ms);
        mock.set_current_uri(Some(uri.to_string())).await;
    }

    /// (lectures envoyées, `Stop` envoyés) à la sortie.
    async fn ordres_recus(&self) -> (usize, u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        (mock.play_call_count().await, mock.stop_call_count())
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    async fn etat(&self) -> PlayState {
        self.playback.get_state(self.zone_id).await.state
    }
}

#[tokio::test]
async fn la_liste_chargee_par_kazoo_reste_intacte_7636() {
    let mut banc = Banc::monter("openhome", "uuid:klimax-7636").await;

    // Prémisse : Tune joue son flux, rien ne bouge.
    banc.sortie(TransportState::Playing, 5_000, URI_DE_TUNE)
        .await;
    banc.tic().await;
    assert_eq!(banc.etat().await, PlayState::Playing, "prémisse");

    // Kazoo charge sa liste et la joue.
    for s in 1..=3u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "un autre point de contrôle tient le renderer OpenHome depuis trois \
         relevés : la zone Tune doit le lâcher. Restée « en lecture », la \
         moindre de ses relances automatiques repasse par play_media, dont le \
         DeleteAll vide la liste que Kazoo vient de charger"
    );
    assert_eq!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("stopped"),
        "l'état enregistré doit être « stopped » : sinon le sondeur « reprend » \
         la lecture de Kazoo comme celle de Tune"
    );

    // Kazoo passe d'une piste à l'autre : un arrêt bref, puis il rejoue.
    for _ in 0..6 {
        banc.sortie(TransportState::Stopped, 0, URI_DE_KAZOO).await;
        banc.tic().await;
    }
    for s in 1..=6u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }

    assert_eq!(
        banc.ordres_recus().await,
        (0, 0),
        "aucune lecture (donc aucun DeleteAll) ni aucun Stop ne doit partir \
         vers un renderer qu'un autre point de contrôle tient"
    );
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "la zone ne reprend pas la lecture de Kazoo à son compte"
    );
}

#[tokio::test]
async fn notre_flux_et_la_suivante_armee_ne_font_rien_lacher_7636() {
    let mut banc = Banc::monter("openhome", "uuid:klimax-7636-b").await;
    for s in 1..=4u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_TUNE)
            .await;
        banc.tic().await;
    }
    for s in 5..=9u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_LA_SUIVANTE)
            .await;
        banc.tic().await;
    }
    assert_eq!(
        banc.ordres_recus().await.1,
        0,
        "aucun Stop envoyé à la sortie"
    );
    assert!(
        banc.playback
            .get_state(banc.zone_id)
            .await
            .now_playing
            .is_some(),
        "un flux Tune, même d'un autre identifiant, n'est pas un autre point de contrôle"
    );
    assert_ne!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("stopped"),
        "la zone n'a pas été lâchée"
    );
}

#[tokio::test]
async fn une_zone_dlna_garde_son_comportement_7636() {
    let mut banc = Banc::monter("dlna", "uuid:renderer-dlna-7636").await;
    for s in 1..=4u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    assert_eq!(
        banc.etat().await,
        PlayState::Playing,
        "la garde est propre à OpenHome, seule sortie dont Tune réécrit la liste"
    );
}
