//! #6772 — gapless DLNA une transition sur deux (CXNv2, TIDAL hi-res,
//! `prefetch_mode` 30 s) : après une avance sans blanc adoptée, rien ne
//! préchargeait la piste suivante.
//!
//! Le préchargement n'avait qu'un lanceur, `play_inner`. L'avance sans blanc
//! (`advance_queue_metadata`, journalisée `avance_gapless_flux_adopte`) le
//! contourne : la préparation sans blanc de la piste N+2 partait de zéro,
//! sortait de son budget de 10 s, et la transition suivante avait un blanc.
//!
//! Le témoin passe par la vraie porte : une file de service [A, B, C], A
//! courante, puis l'avance sans blanc vers B. Le service du banc NOTE chaque
//! `get_track_url` sans rien servir : ce qu'il a noté est exactement ce que
//! le préchargement aurait téléchargé. Il doit avoir demandé C.
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::poller::suivante_bannie_5143::{ServiceQuiNote, titre_qobuz};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Ce que le service a noté, au plus tard après `delai` : la tâche de
/// préchargement est lancée en fond.
async fn demandes_apres(
    demandes: &Arc<std::sync::Mutex<Vec<String>>>,
    delai: Duration,
) -> Vec<String> {
    let fin = tokio::time::Instant::now() + delai;
    loop {
        let vues = demandes.lock().unwrap().clone();
        if !vues.is_empty() || tokio::time::Instant::now() >= fin {
            return vues;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// **LA MESURE DU TICKET.** File Qobuz [A, B, C], A en cours, avance sans
/// blanc vers B : le préchargement doit demander C au service. Avant le
/// correctif, rien n'était demandé — aucun `prefetch_starting` après
/// `avance_gapless_flux_adopte`.
#[tokio::test]
async fn l_avance_sans_blanc_precharge_la_piste_qui_suit() {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("dlna"), Some("uuid:banc-6772"))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[titre_qobuz("A"), titre_qobuz("B"), titre_qobuz("C")],
        )
        .unwrap();

    let playback = Arc::new(PlaybackManager::new());
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "Titre A".into(),
                source: "qobuz".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    playback.update_queue_info(zone_id, 0, 3).await;

    let demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceQuiNote {
        demandes: demandes.clone(),
    }));
    let orchestrator = PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(registre)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );

    orchestrator
        .advance_queue_metadata(zone_id, 1)
        .await
        .unwrap();
    assert_eq!(
        playback.get_state(zone_id).await.queue_position,
        1,
        "l'avance a bien eu lieu"
    );

    assert_eq!(
        demandes_apres(&demandes, Duration::from_secs(5)).await,
        vec!["C".to_string()],
        "après une avance sans blanc, le préchargement doit préparer la piste \
         qui suit la nouvelle courante (C), comme au démarrage d'une lecture"
    );
}
