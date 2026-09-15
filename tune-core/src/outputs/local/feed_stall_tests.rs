use super::{
    FEED_STALL_TIMEOUT, OpenFailure, RingBuf, drain_deadline_for,
    feed_ring_abortable_with_stall_timeout, record_feed_stall_failure,
};
use std::sync::atomic::AtomicBool;

/// La boucle de production, avec son seuil ramené à zéro : un anneau plein
/// que personne ne vide rend le verdict « consommateur mort » — tout de
/// suite, sans dormir cinq secondes.
#[test]
fn un_anneau_que_personne_ne_vide_rend_le_verdict_de_blocage() {
    let ring = RingBuf::new(4);
    ring.push(&[0.0; 4]); // plein, et personne ne tirera jamais
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let debut = std::time::Instant::now();
    assert!(
        !feed_ring_abortable_with_stall_timeout(
            &ring,
            &[0.5f32; 8],
            &rx,
            &paused,
            None,
            std::time::Duration::ZERO,
        ),
        "un anneau plein et jamais vidé doit être déclaré bloqué"
    );
    assert!(
        debut.elapsed() < std::time::Duration::from_secs(1),
        "le seuil injecté doit rendre le verdict sans attendre"
    );
}

/// TÉMOIN VERT : le même appel, sur un anneau qui a de la place, ne
/// déclare rien. Le détecteur ne doit pas devenir un couperet.
#[test]
fn un_anneau_qui_accepte_tout_ne_declare_aucun_blocage() {
    let ring = RingBuf::new(16);
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    assert!(feed_ring_abortable_with_stall_timeout(
        &ring,
        &[0.5f32; 8],
        &rx,
        &paused,
        None,
        std::time::Duration::ZERO,
    ));
    assert_eq!(ring.available(), 8);
}

/// Le seuil réel n'est pas nul : un test qui l'injecterait à zéro partout
/// masquerait une production devenue instantanément couperet.
#[test]
fn le_seuil_de_production_laisse_le_temps_a_la_contre_pression() {
    assert_eq!(FEED_STALL_TIMEOUT, std::time::Duration::from_secs(5));
}

/// Le message doit nommer la sortie, la position où l'écran s'est figé, et
/// le geste à faire. « Une erreur est survenue » ne répare rien.
#[test]
fn le_blocage_dit_la_sortie_la_position_et_le_geste() {
    let slot = std::sync::Mutex::new(None);
    record_feed_stall_failure("CoreAudio", "DAC USB", 2000, &slot);
    let message = slot.lock().unwrap().clone().expect("le canal doit porter");
    assert!(message.contains("DAC USB"), "sortie absente : {message}");
    assert!(
        message.contains("CoreAudio"),
        "transport absent : {message}"
    );
    assert!(
        message.contains("2000 ms"),
        "la position figée est le chiffre qui relie l'écran au journal : {message}"
    );
    assert!(
        message.contains("Relancez la lecture"),
        "le premier geste doit etre une relance : {message}"
    );
    assert!(
        message.contains("essayez une autre sortie"),
        "le repli en cas de repetition est absent : {message}"
    );
    assert!(
        !message.contains(OpenFailure::DeviceGone.user_message())
            && !message.contains("n'est plus là"),
        "un anneau bloque ne prouve pas la disparition du peripherique : {message}"
    );
}

/// Le canal est celui du poller : `take_output_failure()` le draine, une
/// fois, et le tick suivant ne re-stoppe pas la zone.
#[test]
fn le_blocage_passe_par_le_canal_que_le_poller_draine() {
    use super::super::traits::OutputTarget;
    let sortie = super::LocalOutput::new("DAC USB".into());
    assert!(
        sortie.take_output_failure().is_none(),
        "TÉMOIN VERT : une sortie saine ne remonte rien"
    );

    record_feed_stall_failure("CoreAudio", "DAC USB", 2000, &sortie.open_failure);
    let remonte = sortie
        .take_output_failure()
        .expect("le blocage doit remonter par le canal du poller");
    assert!(remonte.contains("2000 ms"), "got: {remonte}");
    assert!(
        sortie.take_output_failure().is_none(),
        "un échec ne doit jamais être remonté deux fois"
    );
}

/// Le vidage borné : durée de l'audio en attente + 5 s de marge.
#[test]
fn le_delai_de_vidage_couvre_l_audio_en_attente_plus_la_marge() {
    // Deux secondes de stéréo à 44,1 kHz = 176 400 échantillons entrelacés.
    assert_eq!(
        drain_deadline_for(44_100 * 2 * 2, 44_100, 2),
        std::time::Duration::from_millis(7000)
    );
    // Anneau vide : la marge seule.
    assert_eq!(
        drain_deadline_for(0, 44_100, 2),
        std::time::Duration::from_millis(5000)
    );
}

/// Une cadence ou un nombre de canaux nuls ne doivent pas diviser par zéro
/// — ce serait tuer le fil de lecture au lieu de borner son vidage.
#[test]
fn une_cadence_nulle_ne_divise_pas_par_zero() {
    assert_eq!(
        drain_deadline_for(0, 0, 0),
        std::time::Duration::from_millis(5000)
    );
}

/// Le nom du diagnostic ne doit pas attribuer le chemin f32 generique a ASIO.
#[test]
fn i4046_la_trace_de_blocage_du_chemin_generique_ne_dit_pas_asio() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let capture = Capture(bytes.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let ring = RingBuf::new(4);
        ring.push(&[0.0; 4]);
        let (_tx, rx) = std::sync::mpsc::channel();
        assert!(!feed_ring_abortable_with_stall_timeout(
            &ring,
            &[0.5; 8],
            &rx,
            &AtomicBool::new(false),
            None,
            std::time::Duration::ZERO,
        ));
    });
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(log.contains("local_audio_feed_ring_stall_timeout"), "{log}");
    assert!(log.contains("remaining_samples=8"), "{log}");
    assert!(!log.contains("asio_"), "{log}");
}

/// LocalOutput -> poller public -> evenement fatal, sans ouvrir de carte son.
#[tokio::test]
async fn i4046_le_message_reel_du_blocage_arrive_au_client_et_arrete_la_zone() {
    use crate::db::{migrations::run_migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};
    use crate::event_bus::EventBus;
    use crate::http::streamer::AudioStreamer;
    use crate::orchestrator::PlaybackOrchestrator;
    use crate::outputs::{registry::OutputRegistry, traits::OutputTarget};
    use crate::playback::{NowPlaying, PlayState, PlaybackManager};
    use crate::poller::PositionPoller;
    use crate::streaming::ServiceRegistry;
    use std::collections::HashMap;
    use std::sync::{Arc, atomic::Ordering};
    use tokio::sync::Mutex;

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let sortie = super::LocalOutput::new("DAC USB".into());
    let device = sortie.device_id().to_string();
    sortie.playing.store(true, Ordering::SeqCst);
    sortie.position_ms.store(2000, Ordering::SeqCst);
    record_feed_stall_failure("CPAL", "DAC USB", 2000, &sortie.open_failure);
    let attendu = sortie.open_failure.lock().unwrap().clone().unwrap();
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon 4046", Some("local"), Some(&device))
        .unwrap();
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
    let bus = Arc::new(EventBus::new());
    let mut recu = bus.subscribe();
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db,
        Arc::new(Mutex::new(HashMap::new())),
    )
    .with_event_bus(bus);
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "Temoin 4046".into(),
                source: "local".into(),
                duration_ms: 240_000,
                ..Default::default()
            },
        )
        .await;
    let task = poller.spawn();
    let resultat = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let event = loop {
            let event = recu.recv().await.unwrap();
            if event.event_type == "zone.playback_error" && event.data["zone_id"] == zone_id {
                break event;
            }
        };
        while playback.get_state(zone_id).await.state == PlayState::Playing {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        event
    })
    .await;
    // Nettoyage avant toute assertion : ce test ne laisse pas de poller vivant.
    task.abort();
    let _ = task.await;
    let event = resultat.expect("le poller doit emettre puis arreter la zone");
    println!(
        "I4046_EVENT={}",
        serde_json::json!({"type": event.event_type, "data": event.data})
    );
    assert_eq!(event.data["error"], attendu);
    assert_eq!(event.data["fatal"], true);
    assert!(attendu.contains("Relancez la lecture"), "{attendu}");
    assert!(!attendu.contains("n'est plus là"), "{attendu}");
}
