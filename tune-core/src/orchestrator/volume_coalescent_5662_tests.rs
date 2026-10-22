//! #5662 — une rafale de changements de volume ne part pas en série vers
//! l'appareil : une commande en vol, la dernière valeur en attente.
//!
//! Le faux renderer attend un verdict explicite du banc et compte ce
//! qu'il reçoit : c'est la seule question qui vaille, « qu'a reçu
//! l'appareil, et quand ? ».
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::{OutputCapabilities, OutputCommand, OutputCommandError};
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-renderer-lent";
const GARDE: Duration = Duration::from_secs(15);

struct RendererLent {
    refuse: bool,
    recus: Arc<std::sync::Mutex<Vec<f64>>>,
    verdicts: Arc<Semaphore>,
}

#[async_trait::async_trait]
impl crate::outputs::traits::OutputTarget for RendererLent {
    fn name(&self) -> &str {
        "Renderer lent"
    }
    fn device_id(&self) -> &str {
        APPAREIL
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }
    async fn play_media(
        &self,
        _media: &crate::outputs::traits::PlayMedia<'_>,
    ) -> Result<(), String> {
        Ok(())
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
    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.recus.lock().unwrap().push(volume);
        self.verdicts.acquire().await.unwrap().forget();
        if self.refuse {
            Err("UPnPError 501 Action Failed".into())
        } else {
            Ok(())
        }
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
        Ok(Default::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    zone_id: i64,
    recus: Arc<std::sync::Mutex<Vec<f64>>>,
    verdicts: Arc<Semaphore>,
    bus: Arc<EventBus>,
}

/// Une zone à 50 % sur un renderer lent, avec un bus d'évènements.
async fn banc(refuse: bool) -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let bus = Arc::new(EventBus::new());
    orch.event_bus = Some(bus.clone());
    let repo = ZoneRepo::with_backend(db);
    let zone_id = repo.create("Salon", Some("dlna"), Some(APPAREIL)).unwrap();
    repo.update_volume(zone_id, 50.0).unwrap();
    orch.playback.set_volume(zone_id, 0.5).await;
    let recus = Arc::new(std::sync::Mutex::new(Vec::new()));
    let verdicts = Arc::new(Semaphore::new(0));
    orch.outputs.lock().await.register(Box::new(RendererLent {
        refuse,
        recus: recus.clone(),
        verdicts: verdicts.clone(),
    }));
    Banc {
        orch: Arc::new(orch),
        zone_id,
        recus,
        verdicts,
        bus,
    }
}

/// Le faux appareil ne rend aucun verdict avant que toute la rafale soit
/// enregistrée. Les demandes remplacées doivent finir pendant qu'il est bloqué.
async fn rafale(banc: &Banc, valeurs: &[f64]) -> Vec<Result<(), OutputCommandError>> {
    tokio::time::timeout(GARDE, async {
        let mut taches = Vec::new();
        let file = banc.orch.volume_coalesceur.file(APPAREIL, banc.zone_id);
        for (index, &v) in valeurs.iter().enumerate() {
            let orch = banc.orch.clone();
            let zone_id = banc.zone_id;
            taches.push(tokio::spawn(async move {
                orch.set_volume(zone_id, v, Some(APPAREIL)).await
            }));
            // L'ordre demandé est établi par l'enregistrement, pas par 3 ms
            // supposées suffisantes pour qu'un worker chargé soit ordonnancé.
            while file.etat.lock().await.dernier_ticket < (index + 1) as u64 {
                tokio::task::yield_now().await;
            }
            if index == 0 {
                while banc.recus.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            }
        }
        let dernier = taches.len() - 1;
        let mut sorties: Vec<_> = (0..taches.len()).map(|_| Ok(())).collect();
        for (index, tache) in taches.iter_mut().enumerate().take(dernier).skip(1) {
            sorties[index] = tache.await.unwrap();
        }
        assert!(
            !taches[0].is_finished(),
            "la première demande doit attendre le verdict"
        );
        assert!(
            !taches[dernier].is_finished(),
            "la dernière demande doit attendre le verdict"
        );
        assert_eq!(
            banc.recus.lock().unwrap().len(),
            1,
            "une seule commande en vol"
        );
        banc.verdicts.add_permits(2);
        sorties[0] = (&mut taches[0]).await.unwrap();
        if dernier > 0 {
            sorties[dernier] = (&mut taches[dernier]).await.unwrap();
        }
        sorties
    })
    .await
    .expect("rafale bloquée : les demandes remplacées doivent finir avant le verdict de l'appareil")
}

fn volume_en_base(banc: &Banc) -> f64 {
    ZoneRepo::with_backend(banc.orch.db.clone())
        .get(banc.zone_id)
        .unwrap()
        .unwrap()
        .volume
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trente_pas_en_rafale_donnent_au_plus_deux_commandes_5662() {
    let banc = banc(false).await;
    let valeurs: Vec<f64> = (0..30).map(|i| 0.10 + 0.01 * i as f64).collect();
    let derniere = *valeurs.last().unwrap();
    let sorties = rafale(&banc, &valeurs).await;

    for r in &sorties {
        assert_eq!(r, &Ok(()), "aucune demande n'échoue");
    }
    let recus = banc.recus.lock().unwrap().clone();
    assert!(
        recus.len() <= 2,
        "au plus deux commandes vers l'appareil pour trente pas : {recus:?}"
    );
    assert_eq!(
        recus.last().copied(),
        Some(derniere),
        "la dernière valeur demandée est celle que l'appareil a reçue en dernier"
    );
    let memoire = banc.orch.playback.get_state(banc.zone_id).await.volume;
    assert!((memoire - derniere).abs() < 1e-9, "mémoire : {memoire}");
    assert!(
        (volume_en_base(&banc) - derniere * 100.0).abs() < 1e-6,
        "base : {}",
        volume_en_base(&banc)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_refus_de_la_derniere_valeur_est_propage_et_la_memoire_retablie_5662() {
    let banc = banc(true).await;
    let mut bus = banc.bus.subscribe();
    let mut volumes = banc.orch.playback.subscribe();
    let valeurs = [0.61, 0.62, 0.63, 0.64, 0.65];
    let sorties = rafale(&banc, &valeurs).await;

    // La dernière valeur est partie, l'appareil l'a refusée : 502 côté route.
    assert!(
        matches!(
            sorties.last().unwrap(),
            Err(OutputCommandError::Failed {
                command: OutputCommand::SetVolume,
                ..
            })
        ),
        "le refus de la dernière valeur est rendu à son demandeur : {:?}",
        sorties.last().unwrap()
    );
    let recus = banc.recus.lock().unwrap().clone();
    assert!(recus.len() <= 2, "commandes : {recus:?}");
    assert_eq!(recus.last().copied(), Some(0.65));

    // Rien n'a été accepté : retour à la valeur d'avant la rafale.
    let memoire = banc.orch.playback.get_state(banc.zone_id).await.volume;
    assert!((memoire - 0.5).abs() < 1e-9, "mémoire rétablie : {memoire}");
    assert!((volume_en_base(&banc) - 50.0).abs() < 1e-6, "base intacte");

    let mut erreurs = Vec::new();
    while let Ok(e) = bus.try_recv() {
        if e.event_type == "zone.playback_error" {
            erreurs.push(e.data["error"].as_str().unwrap_or_default().to_string());
        }
    }
    assert!(
        !erreurs.is_empty() && erreurs.iter().all(|m| m.contains("set_volume")),
        "l'évènement d'erreur nomme set_volume : {erreurs:?}"
    );
    let mut dernier_volume = None;
    while let Ok(e) = volumes.try_recv() {
        if e.event == "volume" {
            dernier_volume = e.data["volume"].as_f64();
        }
    }
    assert_eq!(
        dernier_volume,
        Some(0.5),
        "le dernier playback.volume annonce la valeur rétablie"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_demande_isolee_attend_le_verdict_de_l_appareil_5662() {
    let banc = banc(false).await;
    tokio::time::timeout(GARDE, async {
        let appel = banc.orch.set_volume(banc.zone_id, 0.42, Some(APPAREIL));
        tokio::pin!(appel);
        // Piloter l'appel lui-même : observer seulement is_finished() d'une
        // tâche concurrente pouvait précéder son retour et donner un faux vert.
        std::future::poll_fn(|cx| {
            use std::future::Future;
            use std::task::Poll;
            match appel.as_mut().poll(cx) {
                Poll::Ready(_) => panic!("la demande isolée rend avant le verdict de l'appareil"),
                Poll::Pending if !banc.recus.lock().unwrap().is_empty() => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        banc.verdicts.add_permits(1);
        assert_eq!(appel.await, Ok(()));
    })
    .await
    .expect("la commande isolée doit parvenir au faux appareil");
    assert_eq!(banc.recus.lock().unwrap().clone(), vec![0.42]);
    assert!((volume_en_base(&banc) - 42.0).abs() < 1e-6);
}
