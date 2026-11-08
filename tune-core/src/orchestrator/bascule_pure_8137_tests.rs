//! #8137 — la bascule PURE ne commande le volume de l'appareil que si elle
//! change quelque chose. Les commandes sont comptées SUR L'APPAREIL (sortie
//! factice).
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{EnvoiDeVolume, PlaybackOrchestrator};
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-renderer-8137";

/// Une zone DLNA à 40 %, trim de −2 dB.
async fn zone(volume_fixe: bool) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo.create("Salon", Some("dlna"), Some(APPAREIL)).unwrap();
    repo.update_volume(zone_id, 40.0).unwrap();
    if volume_fixe {
        repo.update_fixed_volume(zone_id, true).unwrap();
    }
    orch.playback.set_volume(zone_id, 0.4).await;
    SettingsRepo::with_backend(db)
        .set(&format!("zone_{zone_id}_gain_trim_db"), "-2")
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
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

#[tokio::test]
async fn volume_fixe_l_entree_en_pure_ne_commande_rien_8137() {
    let (orch, zone_id) = zone(true).await;
    let envoi = orch
        .set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(envoi, EnvoiDeVolume::AucunVolumeFixe);
    let sortie = orch
        .set_volume_sortie_de_pure(zone_id, 1.0, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(sortie, EnvoiDeVolume::AucunVolumeFixe);
    assert_eq!(
        recus(&orch).await,
        Vec::<f64>::new(),
        "#8137 : volume fixe, aucune commande de volume à la bascule"
    );
}

/// L'appareil relu à 100 % : armer PURE n'a rien à lui commander.
#[tokio::test]
async fn appareil_relu_a_100_l_entree_en_pure_ne_commande_rien_8137() {
    let (orch, zone_id) = zone(false).await;
    orch.noter_le_volume_relu(zone_id, 1.0);
    let envoi = orch
        .set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(
        recus(&orch).await,
        Vec::<f64>::new(),
        "#8137 : l'appareil a été relu à 100 %, la bascule ne doit pas lui \
         renvoyer 100 %"
    );
    assert_eq!(envoi, EnvoiDeVolume::AucunDejaALaCible(1.0));
    // La consigne de la zone suit quand même : PURE verrouillé, 100 %.
    assert_eq!(orch.playback.get_state(zone_id).await.volume, 1.0);
    let en_base = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .volume;
    assert_eq!(en_base, 100.0);
}

/// La sortie : l'appareil relu à la consigne recomposée (100 % × −2 dB).
#[tokio::test]
async fn appareil_relu_a_la_consigne_la_sortie_ne_commande_rien_8137() {
    let (orch, zone_id) = zone(false).await;
    let trim = 10f64.powf(-2.0 / 20.0);
    orch.noter_le_volume_relu(zone_id, (trim * 100.0).round() / 100.0);
    orch.playback.set_volume(zone_id, 1.0).await;
    let envoi = orch
        .set_volume_sortie_de_pure(zone_id, 1.0, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(
        recus(&orch).await,
        Vec::<f64>::new(),
        "#8137 : l'appareil est déjà à la consigne recomposée"
    );
    assert!(matches!(envoi, EnvoiDeVolume::AucunDejaALaCible(_)));
}

/// Volume variable, relu ailleurs : la bascule commande comme avant, et la
/// relecture, périmée par la commande, est oubliée.
#[tokio::test]
async fn volume_variable_relu_ailleurs_la_bascule_commande_8137() {
    let (orch, zone_id) = zone(false).await;
    orch.noter_le_volume_relu(zone_id, 0.32);
    let envoi = orch
        .set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(envoi, EnvoiDeVolume::Envoye(1.0));
    let sortie = orch
        .set_volume_sortie_de_pure(zone_id, 1.0, Some(APPAREIL))
        .await
        .unwrap();
    let trim = 10f64.powf(-2.0 / 20.0);
    let recu = recus(&orch).await;
    assert_eq!(recu.len(), 2, "{recu:?}");
    assert_eq!(recu[0], 1.0);
    assert!((recu[1] - trim).abs() < 1e-6, "{recu:?}");
    assert!(sortie.envoye());
}

/// Une commande ordinaire périme la relecture : la bascule suivante commande.
#[tokio::test]
async fn une_commande_perime_la_relecture_8137() {
    let (orch, zone_id) = zone(false).await;
    orch.noter_le_volume_relu(zone_id, 1.0);
    orch.set_volume(zone_id, 0.45, Some(APPAREIL))
        .await
        .unwrap();
    let envoi = orch
        .set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(envoi, EnvoiDeVolume::Envoye(1.0));
    assert_eq!(recus(&orch).await.len(), 2);
}
