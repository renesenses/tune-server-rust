//! #5662 — une rafale de changements de volume ne part pas en série vers
//! l'appareil : une commande en vol, la dernière valeur en attente.
//!
//! Le faux renderer est LENT (chaque `SetVolume` prend 300 ms) et compte ce
//! qu'il reçoit : c'est la seule question qui vaille, « qu'a reçu
//! l'appareil, et quand ? ».
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

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
const LENTEUR: Duration = Duration::from_millis(300);

struct RendererLent {
    refuse: bool,
    recus: Arc<std::sync::Mutex<Vec<f64>>>,
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
        tokio::time::sleep(LENTEUR).await;
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
    orch.outputs.lock().await.register(Box::new(RendererLent {
        refuse,
        recus: recus.clone(),
    }));
    Banc {
        orch: Arc::new(orch),
        zone_id,
        recus,
        bus,
    }
}

/// Lance `valeurs` en rafale (une demande toutes les 3 ms, bien plus vite
/// que l'appareil), et rend, dans l'ordre, le résultat et la durée de chaque
/// demande.
async fn rafale(banc: &Banc, valeurs: &[f64]) -> Vec<(Result<(), OutputCommandError>, Duration)> {
    let mut taches = Vec::new();
    for &v in valeurs {
        let orch = banc.orch.clone();
        let zone_id = banc.zone_id;
        taches.push(tokio::spawn(async move {
            let debut = Instant::now();
            let r = orch.set_volume(zone_id, v, Some(APPAREIL)).await;
            (r, debut.elapsed())
        }));
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    let mut sorties = Vec::new();
    for t in taches {
        sorties.push(t.await.unwrap());
    }
    sorties
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
    let debut = Instant::now();
    let sorties = rafale(&banc, &valeurs).await;
    let total = debut.elapsed();

    for (r, _) in &sorties {
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
    // Les demandes remplacées répondent sans attendre l'appareil.
    let rapides = sorties.iter().filter(|(_, d)| *d < LENTEUR / 2).count();
    assert!(rapides >= 27, "demandes remplacées rapides : {rapides}/30");
    // En série, 30 × 300 ms = 9 s ; ici deux commandes au plus.
    assert!(total < LENTEUR * 8, "rafale entière : {total:?}");
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
            sorties.last().unwrap().0,
            Err(OutputCommandError::Failed {
                command: OutputCommand::SetVolume,
                ..
            })
        ),
        "le refus de la dernière valeur est rendu à son demandeur : {:?}",
        sorties.last().unwrap().0
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
    let debut = Instant::now();
    let sorties = rafale(&banc, &[0.42]).await;
    assert_eq!(sorties[0].0, Ok(()));
    assert!(
        debut.elapsed() >= LENTEUR,
        "une demande seule répond après l'appareil, comme avant"
    );
    assert_eq!(banc.recus.lock().unwrap().clone(), vec![0.42]);
    assert!((volume_en_base(&banc) - 42.0).abs() < 1e-6);
}
