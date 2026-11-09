//! #8006 — un appareil qui ne rend pas sa sortie (démarrage d'un titre qui
//! traîne, ampli Devialet qui s'allume) retenait Pause et Arrêt sans limite.
//!
//! Le banc tient lui-même le verrou de la sortie factice, comme le ferait un
//! `play_media` qui n'en finit pas. En test, l'attente bornée vaut
//! [`ATTENTE_DE_LA_SORTIE`] (400 ms) ; chaque appel est entouré d'une
//! échéance de 3 s, que le code d'avant dépassait toujours.

use super::transport::ATTENTE_DE_LA_SORTIE;
use super::*;
use std::time::Duration;

use crate::db::zone_repo::ZoneRepo;
use crate::outputs::mock::MockOutput;

const APPAREIL: &str = "devialet-8006";
const ECHEANCE: Duration = Duration::from_secs(3);

struct Banc {
    orch: PlaybackOrchestrator,
    sorties: Arc<Mutex<OutputRegistry>>,
    bus: Arc<EventBus>,
    zone: i64,
}

fn banc() -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let sorties = Arc::new(Mutex::new(OutputRegistry::new()));
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        sorties.clone(),
        None,
    );
    let bus = Arc::new(EventBus::new());
    orch.event_bus = Some(bus.clone());
    let zone = ZoneRepo::with_backend(db)
        .create("Salon (banc 8006)", Some("mock"), Some(APPAREIL))
        .unwrap();
    Banc {
        orch,
        sorties,
        bus,
        zone,
    }
}

impl Banc {
    async fn enregistrer(&self) -> Arc<Mutex<Box<dyn crate::outputs::traits::OutputTarget>>> {
        let mut registre = self.sorties.lock().await;
        registre.register(Box::new(MockOutput::new(APPAREIL, "Devialet")));
        registre.get(APPAREIL).unwrap()
    }
}

async fn arrets_recus(sortie: &Arc<Mutex<Box<dyn crate::outputs::traits::OutputTarget>>>) -> u64 {
    sortie
        .lock()
        .await
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .stop_call_count()
}

#[tokio::test]
async fn une_pause_sur_un_appareil_occupe_rend_la_main_et_le_nomme_8006() {
    let b = banc();
    let sortie = b.enregistrer().await;
    let occupee = sortie.lock().await;

    let rendu = tokio::time::timeout(ECHEANCE, b.orch.pause(b.zone, Some(APPAREIL)))
        .await
        .expect(
            "la pause doit rendre la main quand l'appareil est occupé, au lieu \
             d'attendre sa sortie sans limite (#8006)",
        );
    let erreur = rendu.expect_err("témoin : la pause n'a pas pu être donnée");
    assert!(
        erreur.to_string().contains(APPAREIL),
        "l'erreur doit nommer l'appareil qui ne répond pas : {erreur}"
    );
    drop(occupee);
}

#[tokio::test]
async fn un_arret_sur_un_appareil_occupe_rend_la_main_puis_l_arrete_8006() {
    let b = banc();
    let sortie = b.enregistrer().await;
    let mut evenements = b.bus.subscribe();
    let occupee = sortie.lock().await;

    let debut = std::time::Instant::now();
    tokio::time::timeout(ECHEANCE, b.orch.stop(b.zone, Some(APPAREIL)))
        .await
        .expect(
            "l'arrêt doit rendre la main quand l'appareil est occupé, au lieu \
             d'attendre sa sortie sans limite (#8006)",
        );
    assert!(
        debut.elapsed() >= ATTENTE_DE_LA_SORTIE,
        "témoin : l'arrêt a attendu"
    );

    let evenement = loop {
        let e = evenements.try_recv().expect("un zone.playback_error");
        if e.event_type == "zone.playback_error" {
            break e;
        }
    };
    assert_eq!(evenement.data["code"], "device_not_responding");
    assert_eq!(evenement.data["device_id"], APPAREIL);

    // L'appareil se libère : l'arrêt, resté en route, lui parvient.
    drop(occupee);
    let mut recu = 0;
    for _ in 0..100 {
        recu = arrets_recus(&sortie).await;
        if recu > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        recu, 1,
        "l'arrêt doit parvenir à l'appareil dès qu'il se libère, sinon il joue \
         derrière une zone qui se dit arrêtée"
    );
}

#[tokio::test]
async fn contre_epreuve_un_appareil_libre_s_arrete_sans_attendre_8006() {
    let b = banc();
    let sortie = b.enregistrer().await;
    let mut evenements = b.bus.subscribe();

    let debut = std::time::Instant::now();
    b.orch.stop(b.zone, Some(APPAREIL)).await;
    assert!(debut.elapsed() < ATTENTE_DE_LA_SORTIE);
    assert_eq!(arrets_recus(&sortie).await, 1);
    while let Ok(e) = evenements.try_recv() {
        assert_ne!(e.event_type, "zone.playback_error", "{:?}", e.data);
    }
}
