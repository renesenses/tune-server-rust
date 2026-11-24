//! #9315 — le banc : le VRAI sondeur (`tick`), un vrai orchestrateur, une
//! sortie DLNA factice, et la session du mandataire sur laquelle
//! `tune-stream-http` a noté l'erreur de la source.
//!
//! Témoins :
//!  1. source en 503 notée sur la session : au premier tour, la zone
//!     s'arrête avec UN bandeau fatal `source_http_error` (statut, titre) ;
//!     le 404/410 a son propre code, `episode_unavailable` (fil 2972,
//!     témoins en fin de fichier) ;
//!  2. CONTRE-TÉMOIN : la même zone, sans erreur notée, continue de jouer
//!     sans bandeau ;
//!  3. erreur notée PUIS audio servi (le renderer a réessayé après le 502) :
//!     la zone continue, même si l'erreur n'a pas encore été effacée ;
//!  4. erreur notée puis effacée par un 2xx de la source : la zone continue.
//!
//! Contre-épreuves :
//!  - retirer le bloc « #9315 » de `tick.rs` fait tomber le témoin 1 (aucun
//!    bandeau, la zone reste en lecture) ;
//!  - retirer la condition `stream_bytes_sent(sid) == Some(0)` du bloc fait
//!    tomber le témoin 3 (la zone qui joue est arrêtée).
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::outputs::traits::TransportState;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "uuid:dmp-a6-9315";
const TITRE: &str = "Concert de l'après-midi";

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    streamer: Arc<AudioStreamer>,
    session: String,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

impl Banc {
    /// Une zone DLNA qui joue un épisode relayé par le mandataire.
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Eversolo DMP-A6").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let streamer = Arc::new(AudioStreamer::new(0));
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            streamer.clone(),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let session = streamer
            .create_proxy_session(
                StreamInfo {
                    format: "m4a".into(),
                    mime_type: "audio/mp4".into(),
                    channels: 2,
                    ..Default::default()
                },
                "http://127.0.0.1:9/episode-9315.m4a".into(),
                false,
            )
            .await;
        playback
            .play(
                zone_id,
                NowPlaying {
                    title: TITRE.into(),
                    source: "podcast".into(),
                    source_id: Some("http://127.0.0.1:9/episode-9315.m4a".into()),
                    stream_id: Some(session.clone()),
                    duration_ms: 3_600_000,
                    ..Default::default()
                },
            )
            .await;
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db,
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);
        Self {
            poller,
            playback,
            outputs,
            streamer,
            session,
            zone_id,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
            recu,
        }
    }

    /// Le renderer dit « en lecture », à 0 : il vient de recevoir le 502.
    async fn sortie_en_lecture(&self) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(TransportState::Playing).await;
        mock.set_duration(3_600_000);
        mock.set_position(0);
    }

    async fn noter_l_erreur_de_la_source(&self, statut: u16) {
        let sessions = self.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(&self.session)
            .expect("la session du mandataire existe")
            .signaler_erreur_amont(statut);
    }

    async fn session(&self) -> Arc<crate::http::streamer::StreamSession> {
        self.streamer
            .sessions_state()
            .lock()
            .await
            .get(&self.session)
            .cloned()
            .expect("la session du mandataire existe")
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    fn bandeaux(&mut self) -> Vec<serde_json::Value> {
        let mut v = Vec::new();
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.playback_error"
                && ev.data.get("zone_id").and_then(|z| z.as_i64()) == Some(self.zone_id)
            {
                v.push(ev.data.clone());
            }
        }
        v
    }
}

#[tokio::test]
async fn une_source_en_503_arrete_la_zone_avec_un_bandeau_9315() {
    let mut banc = Banc::monter().await;
    banc.sortie_en_lecture().await;
    banc.noter_l_erreur_de_la_source(503).await;
    banc.tic().await;

    let bandeaux = banc.bandeaux();
    assert_eq!(
        bandeaux.len(),
        1,
        "#9315 : UN bandeau doit dire que la source a refusé : {bandeaux:?}"
    );
    let b = &bandeaux[0];
    assert_eq!(
        b["code"],
        source_en_erreur_9315::CODE_SOURCE_EN_ERREUR,
        "{b}"
    );
    assert_eq!(b["params"]["status"], 503, "{b}");
    assert_eq!(b["params"]["title"], TITRE, "{b}");
    assert_eq!(b["fatal"], true, "{b}");
    assert!(b["error"].as_str().unwrap().contains("HTTP 503"), "{b}");
    assert_ne!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "#9315 : la zone ne reste pas « en lecture » sur une source morte"
    );

    // Plus rien ensuite : la zone est arrêtée.
    banc.tic().await;
    assert!(banc.bandeaux().is_empty());
}

#[tokio::test]
async fn sans_erreur_de_la_source_la_zone_continue_9315() {
    let mut banc = Banc::monter().await;
    banc.sortie_en_lecture().await;
    banc.tic().await;
    assert!(banc.bandeaux().is_empty());
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing
    );
}

#[tokio::test]
async fn une_erreur_suivie_d_audio_servi_laisse_jouer_9315() {
    let mut banc = Banc::monter().await;
    banc.sortie_en_lecture().await;
    banc.noter_l_erreur_de_la_source(503).await;
    // Le renderer a réessayé et reçu l'audio.
    banc.session().await.compter_octets_servis(64 * 1024);
    banc.tic().await;
    assert!(
        banc.bandeaux().is_empty(),
        "#9315 : aucun bandeau pour une zone qui reçoit l'audio"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "#9315 : la zone qui joue n'est pas arrêtée"
    );
}

#[tokio::test]
async fn une_erreur_effacee_par_un_succes_laisse_jouer_9315() {
    let mut banc = Banc::monter().await;
    banc.sortie_en_lecture().await;
    banc.noter_l_erreur_de_la_source(503).await;
    banc.session().await.effacer_erreur_amont();
    banc.tic().await;
    assert!(banc.bandeaux().is_empty());
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing
    );
}

// ── Fil 2972 — l'éditeur ne sert plus le fichier de l'épisode (404/410) ──
//
// Contre-épreuves : retirer l'appel à `episodes_indisponibles::marquer` de
// `arreter_sur_source_en_erreur` fait tomber la marque du témoin 404 ;
// retirer la branche 404/410 de `charge_source_en_erreur` fait retomber le
// code sur `source_http_error`.

const ADRESSE: &str = "http://127.0.0.1:9/episode-9315.m4a";

#[tokio::test]
async fn un_episode_en_404_chez_l_editeur_est_indisponible_sans_boucle_2972() {
    use crate::streaming::episodes_indisponibles as indispo;
    indispo::oublier(ADRESSE);
    let mut banc = Banc::monter().await;
    banc.sortie_en_lecture().await;
    banc.noter_l_erreur_de_la_source(404).await;
    banc.tic().await;

    let bandeaux = banc.bandeaux();
    assert_eq!(bandeaux.len(), 1, "fil 2972 : UN bandeau : {bandeaux:?}");
    let b = &bandeaux[0];
    assert_eq!(b["code"], indispo::CODE_EPISODE_INDISPONIBLE, "{b}");
    assert_eq!(b["params"]["status"], 404, "{b}");
    assert_eq!(b["fatal"], true, "{b}");
    assert_ne!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "fil 2972 : la zone s'arrête au lieu de faire semblant de jouer"
    );
    assert_eq!(
        indispo::statut_note(ADRESSE),
        Some(404),
        "fil 2972 : l'épisode est marqué indisponible pour les listes"
    );
    // Aucune relance : les tours suivants ne rejouent rien et ne disent rien.
    let lancements = {
        let reg = banc.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_titles()
            .await
            .len()
    };
    for _ in 0..3 {
        banc.tic().await;
    }
    assert!(
        banc.bandeaux().is_empty(),
        "fil 2972 : pas de boucle de bandeaux"
    );
    let reg = banc.outputs.lock().await;
    let arc = reg.get(APPAREIL).unwrap();
    let sortie = arc.lock().await;
    assert_eq!(
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_titles()
            .await
            .len(),
        lancements,
        "fil 2972 : aucune relance de l'épisode"
    );
    drop(sortie);
    drop(reg);
    indispo::oublier(ADRESSE);
}

#[tokio::test]
async fn une_source_en_503_ne_marque_pas_l_episode_2972() {
    use crate::streaming::episodes_indisponibles as indispo;
    let mut banc = Banc::monter().await;
    // Adresse propre à ce témoin : le registre est partagé entre tests.
    let adresse = "http://127.0.0.1:9/episode-2972-503.m4a";
    banc.playback
        .play(
            banc.zone_id,
            NowPlaying {
                title: TITRE.into(),
                source: "podcast".into(),
                source_id: Some(adresse.into()),
                stream_id: Some(banc.session.clone()),
                duration_ms: 3_600_000,
                ..Default::default()
            },
        )
        .await;
    banc.sortie_en_lecture().await;
    banc.noter_l_erreur_de_la_source(503).await;
    banc.tic().await;
    let bandeaux = banc.bandeaux();
    assert_eq!(bandeaux.len(), 1, "{bandeaux:?}");
    assert_eq!(
        bandeaux[0]["code"],
        source_en_erreur_9315::CODE_SOURCE_EN_ERREUR
    );
    assert_eq!(
        indispo::statut_note(adresse),
        None,
        "fil 2972 : une panne passagère (503) ne rend pas l'épisode indisponible"
    );
}
