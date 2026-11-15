//! #8610 — `set_volume` ne lève jamais la sourdine de l'appareil AirPlay 2 :
//! une limite de sécurité abaissée (ou une alarme, le sondeur) gardait la
//! zone « en sourdine » à l'écran pendant que le récepteur rejouait. Seul le
//! geste de volume (#8165, `lever_la_sourdine_au_geste_de_volume`) la lève.
//!
//! Banc sans son : un faux `airplay-daemon` branché sur l'entrée de la
//! sortie, qui note chaque niveau `volume` que Tune lui donne (le niveau que
//! le récepteur jouerait).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use super::{Airplay2Output, DaemonProcess, PairingPhase};
use crate::audio::garde_de_hausse::Hausse;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

type Niveaux = Arc<Mutex<Vec<f64>>>;

fn enceinte(device_id: &str) -> Airplay2Output {
    Airplay2Output::new(
        "Enceinte 8610".into(),
        "127.0.0.1".into(),
        7000,
        device_id.into(),
        "00:11:22:33:44:66".into(),
    )
}

/// Branche un faux démon connecté : rend les niveaux `volume` reçus.
async fn faux_demon(sortie: &Airplay2Output) -> Niveaux {
    let (stdin_tune, stdin_demon) = tokio::io::duplex(4096);
    *sortie.process.lock().await = Some(DaemonProcess {
        child: None,
        stdin: Box::new(stdin_tune),
    });
    *sortie.pairing.lock().await = PairingPhase::Connected;
    let niveaux: Niveaux = Arc::new(Mutex::new(Vec::new()));
    let notes = niveaux.clone();
    tokio::spawn(async move {
        let mut lignes = BufReader::new(stdin_demon).lines();
        while let Ok(Some(ligne)) = lignes.next_line().await {
            let commande: serde_json::Value = serde_json::from_str(&ligne).unwrap();
            if commande["cmd"] == "volume" {
                notes.lock().await.push(commande["level"].as_f64().unwrap());
            }
        }
    });
    niveaux
}

/// Ce que le récepteur joue : le dernier niveau reçu.
async fn joue(niveaux: &Niveaux) -> Option<f64> {
    // Le faux démon lit dans sa propre tâche : on lui laisse la main.
    tokio::time::sleep(Duration::from_millis(50)).await;
    niveaux.lock().await.last().copied()
}

fn proche(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-6)
}

/// La sortie seule : en sourdine, une consigne est gardée, rien ne part ; la
/// levée de sourdine rend la NOUVELLE consigne.
#[tokio::test]
async fn set_volume_garde_la_sourdine_de_l_appareil_8610() {
    let sortie = enceinte("airplay2:banc-8610-sortie");
    let niveaux = faux_demon(&sortie).await;
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    assert!(proche(joue(&niveaux).await, 0.0));

    sortie.set_volume(0.25).await.unwrap();
    assert!(
        proche(joue(&niveaux).await, 0.0),
        "#8610 : l'appareil reste muet ; reçu : {:?}",
        niveaux.lock().await
    );
    let statut = sortie.get_status().await.unwrap();
    assert!(statut.muted, "#8610 : la sortie reste en sourdine");
    assert!((statut.volume - 0.25).abs() < 1e-9, "consigne gardée");

    sortie.set_mute(false).await.unwrap();
    assert!(
        proche(joue(&niveaux).await, 0.25),
        "{:?}",
        niveaux.lock().await
    );
}

/// Hors sourdine, la consigne part comme avant.
#[tokio::test]
async fn hors_sourdine_la_consigne_part_8610() {
    let sortie = enceinte("airplay2:banc-8610-sonore");
    let niveaux = faux_demon(&sortie).await;
    sortie.set_volume(0.30).await.unwrap();
    assert!(proche(joue(&niveaux).await, 0.30));
    assert!(!sortie.get_status().await.unwrap().muted);
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Zone AirPlay 2 à 40 %, mise en sourdine par l'orchestrateur.
async fn banc(device_id: &str) -> (PlaybackOrchestrator, i64, Niveaux) {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon 8610", Some("airplay2"), Some(device_id))
        .unwrap();
    let sortie = enceinte(device_id);
    let niveaux = faux_demon(&sortie).await;
    orch.outputs.lock().await.register(Box::new(sortie));
    orch.playback.set_volume(zone_id, 0.40).await;
    orch.set_volume(zone_id, 0.40, Some(device_id))
        .await
        .unwrap();
    assert!(proche(joue(&niveaux).await, 0.40), "banc : 40 %");
    orch.set_mute(zone_id, true, Some(device_id)).await.unwrap();
    assert!(proche(joue(&niveaux).await, 0.0), "banc : muette");
    (orch, zone_id, niveaux)
}

fn muette_en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> bool {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .muted
}

/// ⭐ Le cas de l'issue : zone en sourdine, la limite de sécurité descend
/// sous le volume courant. La route (`limites_de_volume_6541`) ramène la
/// zone par `set_volume` : l'appareil reste muet, la zone aussi.
#[tokio::test]
async fn une_limite_abaissee_garde_l_appareil_en_sourdine_8610() {
    let appareil = "airplay2:banc-8610-limite";
    let (orch, zone_id, niveaux) = banc(appareil).await;

    SettingsRepo::with_backend(orch.db.clone())
        .set(&crate::audio::volume_limites::cle_securite(zone_id), "0.25")
        .unwrap();
    orch.set_volume(zone_id, 0.25, Some(appareil))
        .await
        .unwrap();

    assert!(
        proche(joue(&niveaux).await, 0.0),
        "#8610 : la limite ne rend pas le son ; reçu : {:?}",
        niveaux.lock().await
    );
    assert!(muette_en_base(&orch, zone_id));
    assert!(orch.playback.get_state(zone_id).await.muted);
}

/// Le geste de volume (#8165) lève la sourdine : le récepteur rejoue, au
/// nouveau niveau.
#[tokio::test]
async fn un_geste_de_volume_leve_la_sourdine_de_l_appareil_8610() {
    let appareil = "airplay2:banc-8610-geste";
    let (orch, zone_id, niveaux) = banc(appareil).await;

    orch.set_volume_avec_garde(zone_id, 0.20, Some(appareil), Hausse::Gardee)
        .await
        .unwrap();
    assert!(
        proche(joue(&niveaux).await, 0.0),
        "la consigne seule ne lève rien"
    );
    assert!(
        orch.lever_la_sourdine_au_geste_de_volume(zone_id, Some(appareil))
            .await
    );

    assert!(
        proche(joue(&niveaux).await, 0.20),
        "#8165 : le geste rend le son au nouveau volume ; reçu : {:?}",
        niveaux.lock().await
    );
    assert!(!muette_en_base(&orch, zone_id));
    assert!(!orch.playback.get_state(zone_id).await.muted);
}
