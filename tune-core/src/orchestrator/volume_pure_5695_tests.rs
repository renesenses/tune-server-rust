//! #5695 — en PURE verrouillé (« Forcer à 100 % »), l'appareil reçoit 100 %,
//! sans trim de gain, quel que soit le chemin de la commande.
//!
//! Fil 2119 : un Devialet en DLNA, PURE forcé, et `Volume 83%` au chemin du
//! signal. Un trim de −1,6 dB donne exactement 0,8318 : `set_volume`
//! le composait même sous PURE, alors qu'`arm_fixed_volume` l'exclut
//! volontairement. Les commandes sont comptées SUR L'APPAREIL (sortie
//! factice) : c'est la seule question qui vaille, « qu'a reçu le Devialet ? ».
use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-my-devialet";
/// Le trim qui donne exactement les 83 % du fil 2119.
const TRIM_DB: &str = "-1.6";

/// Une zone DLNA sur une sortie factice, à 30 %, avec le trim de −1,6 dB.
async fn zone_devialet(pure_verrouille: bool) -> (PlaybackOrchestrator, i64) {
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
    let zone_id = repo
        .create("My Devialet", Some("dlna"), Some(APPAREIL))
        .unwrap();
    repo.update_volume(zone_id, 30.0).unwrap();
    let reglages = SettingsRepo::with_backend(db);
    reglages
        .set(&format!("zone_{zone_id}_gain_trim_db"), TRIM_DB)
        .unwrap();
    if pure_verrouille {
        reglages
            .set(
                &format!("zone_{zone_id}_audiophile"),
                r#"{"enabled":true,"lock_volume":true}"#,
            )
            .unwrap();
    }
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "My Devialet").with_type("dlna"),
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

fn volume_en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> f64 {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .volume
}

/// LE défaut du fil 2119 : PURE forcé, consigne 100 %, et l'appareil
/// recevait 0,83 parce que le trim était composé.
#[tokio::test]
async fn en_pure_force_l_appareil_recoit_100_sans_trim_5695() {
    let (orch, zone_id) = zone_devialet(true).await;
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    assert_eq!(
        recus(&orch).await,
        vec![1.0],
        "PURE forcé : le trim de {TRIM_DB} dB ne doit pas être composé"
    );
    assert_eq!(volume_en_base(&orch, zone_id), 100.0);
}

/// Le verrou mord sur TOUS les chemins, pas seulement `POST
/// /playback/{id}/volume` : `PUT /zones/{id}/volume` (le client web), le
/// PATCH de zone ou le volume de groupe passaient une consigne basse telle
/// quelle.
#[tokio::test]
async fn en_pure_force_une_consigne_basse_est_ramenee_a_100_5695() {
    let (orch, zone_id) = zone_devialet(true).await;
    orch.set_volume(zone_id, 0.3, Some(APPAREIL)).await.unwrap();
    assert_eq!(recus(&orch).await, vec![1.0]);
    assert_eq!(volume_en_base(&orch, zone_id), 100.0);
    assert_eq!(orch.playback.get_state(zone_id).await.volume, 1.0);
}

/// L'armement de PURE commande le 100 % AVANT d'écrire le réglage : ce
/// chemin-là ne peut pas lire le verrou en base, et ne compose pas de trim.
#[tokio::test]
async fn l_armement_de_pure_commande_100_sans_trim_5695() {
    let (orch, zone_id) = zone_devialet(false).await;
    orch.set_volume_pure_force(zone_id, Some(APPAREIL))
        .await
        .unwrap();
    assert_eq!(recus(&orch).await, vec![1.0]);
    assert_eq!(volume_en_base(&orch, zone_id), 100.0);
}

/// TÉMOIN : hors PURE, le même banc compose bien le trim — 100 % × −1,6 dB
/// part à 83 %. Sans lui, les tests ci-dessus pourraient être verts sur un
/// banc où le trim n'est jamais lu.
#[tokio::test]
async fn temoin_hors_pure_le_trim_fait_partir_83_pourcent() {
    let (orch, zone_id) = zone_devialet(false).await;
    orch.set_volume(zone_id, 1.0, Some(APPAREIL)).await.unwrap();
    let recus = recus(&orch).await;
    assert_eq!(recus.len(), 1);
    assert_eq!(
        (recus[0] * 100.0).round(),
        83.0,
        "100 % × −1,6 dB = {} : c'est le 83 % du fil 2119",
        recus[0]
    );
    // La base garde la consigne de l'utilisateur, jamais la valeur tronquée.
    assert_eq!(volume_en_base(&orch, zone_id), 100.0);
}
