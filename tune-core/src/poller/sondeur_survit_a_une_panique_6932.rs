//! #6932 — une panique dans un tour ne tue plus le sondeur.
//!
//! Fil 2417 (Reivax66, v1.0.0-rc7, Windows) : « La barre d'avancement de
//! lecture en cours ne bouge pas. La lecture s'arrête au bout d'un titre, que
//! ce soit en local ou à distance. » Les deux symptômes ensemble, sur toutes
//! les zones, sont ceux d'un sondeur arrêté : c'est lui qui relève la position
//! et qui constate la fin de piste. Lancé une seule fois au démarrage, sans
//! surveillance, il mourait à la première panique d'un tour.
//!
//! Le témoin monte le VRAI sondeur par `spawn`, avec une sortie factice `dlna`
//! dont le premier relevé panique, puis qui joue à 5 s : la position doit être
//! publiée par un tour suivant.
//!
//! Contre-épreuve : dans `spawn`, remplacer `Self::tour_protege(tour).await`
//! par `tour.await` (le tour sans `catch_unwind`) fait tomber
//! `une_panique_dans_un_tour_ne_fige_pas_la_position`.

use super::*;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "uuid:temoin-6932";
const DUREE_MS: u64 = 240_000;
const POSITION_MS: u64 = 5_000;

#[tokio::test]
async fn une_panique_dans_un_tour_ne_fige_pas_la_position() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon 6932", Some("dlna"), Some(APPAREIL))
        .unwrap();
    let sortie = MockOutput::new(APPAREIL, "Salon 6932").with_type("dlna");
    sortie.set_state(TransportState::Playing).await;
    sortie.set_duration(DUREE_MS);
    sortie.set_position(POSITION_MS);
    sortie.paniquer_au_statut(1);
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(sortie));
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
        outputs,
        db,
        Arc::new(Mutex::new(HashMap::new())),
    );
    playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(6932),
                title: "Témoin 6932".into(),
                source: "local".into(),
                duration_ms: DUREE_MS as i64,
                ..Default::default()
            },
        )
        .await;

    let task = poller.spawn();
    let publiee = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let position = playback.get_state(zone_id).await.position_ms;
            if position >= POSITION_MS as i64 {
                break position;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // Nettoyage avant toute assertion : ce test ne laisse pas de sondeur vivant.
    let mort = task.is_finished();
    task.abort();
    let _ = task.await;

    assert!(
        !mort,
        "le sondeur est mort de la panique d'un seul tour : plus aucune \
         position relevée ni fin de piste constatée sur aucune zone (#6932)"
    );
    assert!(
        publiee.is_ok(),
        "après la panique d'un tour, la position de l'appareil ({POSITION_MS} ms) \
         n'est jamais publiée : la barre d'avancement reste figée et la fin de \
         piste n'est plus vue (#6932)"
    );
}

#[test]
fn la_premiere_panique_puis_une_sur_soixante_sont_journalisees() {
    assert!(decisions::panique_du_tour_a_journaliser(1));
    assert!(!decisions::panique_du_tour_a_journaliser(2));
    assert!(!decisions::panique_du_tour_a_journaliser(59));
    assert!(decisions::panique_du_tour_a_journaliser(60));
    assert!(decisions::panique_du_tour_a_journaliser(120));
}
