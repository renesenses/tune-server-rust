//! #6721 — la garde générale contre les montées brusques, au point de passage
//! unique de l'orchestrateur, vue depuis la SORTIE FACTICE : « qu'a reçu
//! l'appareil ? ». Fil 2366 : un Marantz NR1200 parti à 100 % sur un geste.
use std::sync::Arc;

use serde_json::json;
use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::audio::garde_de_hausse::{Hausse, SEUIL_DE_HAUSSE_DB};
use crate::audio::volume_scale::{EchelleDeVolume, PLAGE_PERCEPTIVE_DB, linear_to_db};
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-marantz-6721";

/// Une zone sur une sortie factice de type `type_sortie`, au volume `depart`
/// en base ET en mémoire (ce que fait `restore_zone_volumes` au démarrage).
async fn zone(type_sortie: &str, depart: f64) -> (Arc<PlaybackOrchestrator>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    ));
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Marantz NR1200", Some(type_sortie), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, depart * 100.0).unwrap();
    orch.playback.set_volume(zone_id, depart).await;
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Marantz NR1200").with_type(type_sortie),
    ));
    (orch, zone_id)
}

async fn recus(orch: &PlaybackOrchestrator) -> Vec<f64> {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(APPAREIL).expect("sortie enregistrée");
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

fn en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> f64 {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .volume
        / 100.0
}

/// Sur un appareil réseau, le plafond est 16,7 points de curseur au-dessus.
const POINTS_DU_SEUIL: f64 = SEUIL_DE_HAUSSE_DB / PLAGE_PERCEPTIVE_DB;

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// LE cas du fil : 10 % → 100 % d'un coup sur un appareil réseau. L'appareil
/// ne reçoit jamais plus que 10 dB perçus au-dessus.
#[tokio::test]
async fn de_10_a_100_l_appareil_ne_recoit_que_le_plafond_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    let retenu = orch
        .set_volume_avec_garde(zone_id, 1.0, Some(APPAREIL), Hausse::Gardee)
        .await
        .unwrap();
    let plafond = 0.10 + POINTS_DU_SEUIL;
    assert!(retenu.hausse_plafonnee, "{retenu:?}");
    assert!(proche(retenu.retenu, plafond), "{retenu:?}");
    let recu = recus(&orch).await;
    assert_eq!(recu.len(), 1, "{recu:?}");
    assert!(
        recu[0] <= plafond + 1e-9,
        "#6721 : l'appareil a reçu {} pour une zone à 10 % — la garde doit \
         plafonner toute hausse à {SEUIL_DE_HAUSSE_DB} dB perçus",
        recu[0]
    );
    assert!(proche(en_base(&orch, zone_id), plafond));
    assert!(proche(
        orch.playback.get_state(zone_id).await.volume,
        plafond
    ));
}

/// `set_volume`, le chemin de TOUTES les autres routes (PATCH, groupes,
/// alarmes, UPnP…), est gardé lui aussi.
#[tokio::test]
async fn set_volume_sans_confirmation_est_garde_6721() {
    let (orch, zone_id) = zone("dlna", 0.0).await;
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    let recu = recus(&orch).await;
    assert!(
        recu.iter().all(|v| *v <= POINTS_DU_SEUIL + 1e-9),
        "#6721 : depuis le silence, l'appareil ne doit pas recevoir plus de \
         {POINTS_DU_SEUIL} : {recu:?}"
    );
}

/// Une hausse CONFIRMÉE passe en entier.
#[tokio::test]
async fn une_hausse_confirmee_passe_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    let retenu = orch
        .set_volume_avec_garde(zone_id, 0.9, Some(APPAREIL), Hausse::Confirmee)
        .await
        .unwrap();
    assert!(!retenu.hausse_plafonnee);
    assert_eq!(recus(&orch).await, vec![0.9]);
}

/// Une baisse, même grande, passe toujours telle quelle.
#[tokio::test]
async fn une_baisse_passe_toujours_6721() {
    let (orch, zone_id) = zone("dlna", 0.9).await;
    orch.set_volume(zone_id, 0.05, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![0.05]);
}

/// Un glisser du curseur (une consigne par mouvement, par petits pas) n'est
/// jamais retenu : la garde ne vise que la hausse « en une fois ».
#[tokio::test]
async fn un_glisser_par_petits_pas_n_est_pas_retenu_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    let mut v = 0.10;
    while v < 1.0 {
        v = (v + 0.05f64).min(1.0);
        let retenu = orch
            .set_volume_avec_garde(zone_id, v, Some(APPAREIL), Hausse::Gardee)
            .await
            .unwrap();
        assert!(!retenu.hausse_plafonnee, "{v} : {retenu:?}");
    }
    assert!(proche(*recus(&orch).await.last().unwrap(), 1.0));
}

/// Sortie logicielle (`volume_curve: db60`) : la hausse se mesure en dB de
/// gain, au plus `SEUIL_DE_HAUSSE_DB`.
#[tokio::test]
async fn sur_une_sortie_logicielle_au_plus_10_db_de_gain_6721() {
    let depart = EchelleDeVolume::Perceptive.gain(0.5); // −30 dB
    let (orch, zone_id) = zone("local", depart).await;
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    let recu = *recus(&orch).await.last().unwrap();
    let hausse = linear_to_db(recu).unwrap() - linear_to_db(depart).unwrap();
    assert!(
        hausse <= SEUIL_DE_HAUSSE_DB + 1e-6,
        "#6721 : +{hausse} dB d'un coup sur une sortie logicielle"
    );
    assert!(hausse > SEUIL_DE_HAUSSE_DB - 1e-3, "{hausse}");
}

/// Les boutons + / − (`set_volume_au_pas`) : un pas démesuré est retenu.
#[tokio::test]
async fn un_pas_demesure_est_retenu_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    orch.set_volume_au_pas(zone_id, 0.9, Some(APPAREIL))
        .await
        .unwrap();
    assert!(*recus(&orch).await.last().unwrap() <= 0.10 + POINTS_DU_SEUIL + 1e-9);
}

/// L'armement confirmé de PURE (100 %) n'est pas retenu par la garde.
#[tokio::test]
async fn l_armement_de_pure_commande_toujours_100_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    orch.set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![1.0]);
}

/// L'assistant IA (`set_volume` de l'outil) passe par le même point de
/// passage : un « mets à fond » ne monte pas une zone de 10 % à 100 %.
#[tokio::test]
async fn l_assistant_est_garde_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    let mut assistant = crate::ai::executor::ToolExecutor::with_backend(
        orch.db.clone(),
        orch.clone(),
        orch.playback.clone(),
        zone_id,
    );
    let _ = assistant
        .execute("set_volume", json!({ "volume": 1.0 }))
        .await;
    let recu = recus(&orch).await;
    assert!(
        recu.iter().all(|v| *v <= 0.10 + POINTS_DU_SEUIL + 1e-9),
        "#6721 : l'assistant a fait recevoir {recu:?} à une zone à 10 %"
    );
}

/// Les limites #6541 tiennent toujours sous une hausse confirmée.
#[tokio::test]
async fn la_securite_tient_sous_une_hausse_confirmee_6721() {
    let (orch, zone_id) = zone("dlna", 0.10).await;
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_volume_limit_safety"), "0.5")
        .unwrap();
    orch.set_volume_avec_garde(zone_id, 1.0, Some(APPAREIL), Hausse::Confirmee)
        .await
        .unwrap();
    assert!(*recus(&orch).await.last().unwrap() <= 0.5 + 1e-9);
}
