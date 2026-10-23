//! #6774 suite — vu de l'APPAREIL : une zone en PURE verrouillé (« Forcer à
//! 100 % ») ou à volume fixe qui passe sur un AUTRE appareil ne lui envoie
//! JAMAIS 100 % du seul fait de l'ancien (décision de Bertrand du 22/10).
//!
//! Les commandes sont comptées sur la sortie factice du NOUVEL appareil : la
//! graine d'une sortie qui naît (`ensemencer_le_volume_local`), puis un geste
//! de volume ordinaire (`set_volume`).
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

const ANCIEN: &str = "dlna-ancien-6774";
const NOUVEAU: &str = "dlna-nouveau-6774";

#[derive(Clone, Copy)]
enum Mode {
    PureVerrouille,
    VolumeFixe,
}

/// Zone sur l'ANCIEN appareil, armée à 100 %, limite de confort à 40 %. Les
/// deux sorties factices sont enregistrées.
async fn zone_armee(mode: Mode) -> (PlaybackOrchestrator, i64) {
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
        .create("Salon 6774", Some("dlna"), Some(ANCIEN))
        .unwrap();
    repo.update_volume(zone_id, 100.0).unwrap();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(&format!("zone_{zone_id}_volume_limit_comfort"), "0.4")
        .unwrap();
    match mode {
        Mode::PureVerrouille => reglages
            .set(
                &format!("zone_{zone_id}_audiophile"),
                r#"{"enabled":true,"lock_volume":true}"#,
            )
            .unwrap(),
        Mode::VolumeFixe => repo.update_fixed_volume(zone_id, true).unwrap(),
    }
    orch.playback.set_volume(zone_id, 1.0).await;
    {
        let mut sorties = orch.outputs.lock().await;
        sorties.register(Box::new(
            MockOutput::new(ANCIEN, "Ancien").with_type("dlna"),
        ));
        sorties.register(Box::new(
            MockOutput::new(NOUVEAU, "Nouveau").with_type("dlna"),
        ));
    }
    (orch, zone_id)
}

/// Ce que la sortie enregistrée `appareil` a reçu.
async fn recus_par(orch: &PlaybackOrchestrator, appareil: &str) -> Vec<f64> {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(appareil).expect("sortie enregistrée");
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

/// La zone passe sur le NOUVEL appareil (comme le PATCH, le hot-swap…),
/// puis le nouvel appareil reçoit sa graine et un geste à 30 %.
async fn passer_sur(orch: &PlaybackOrchestrator, zone_id: i64, appareil: &str) -> (Vec<f64>, bool) {
    ZoneRepo::with_backend(orch.db.clone())
        .update_output_device(zone_id, appareil)
        .unwrap();
    crate::audio::reglages_de_volume::aligner_la_lecture(&orch.db, &orch.playback, zone_id).await;
    let zone = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let graine = MockOutput::new(appareil, "Graine");
    super::ensemencer_le_volume_local(&orch.db, &zone, appareil, &graine).await;
    let mut recus = graine.volume_calls().await;
    let geste_accepte = orch.set_volume(zone_id, 0.3, Some(appareil)).await.is_ok();
    recus.extend(recus_par(orch, appareil).await);
    (recus, geste_accepte)
}

async fn aucune_consigne_a_100_sur_le_nouvel_appareil(mode: Mode, quoi: &str) {
    let (orch, zone_id) = zone_armee(mode).await;
    let (recus, geste_accepte) = passer_sur(&orch, zone_id, NOUVEAU).await;
    assert!(
        !recus.contains(&1.0),
        "#6774 : {quoi} de l'ancien appareil a envoyé 100 % au NOUVEL appareil — reçu {recus:?}"
    );
    assert!(
        geste_accepte,
        "#6774 : {quoi} tient encore le nouvel appareil"
    );
    assert_eq!(
        recus,
        vec![0.4, 0.3],
        "graine à la limite de confort, puis le geste de l'utilisateur"
    );
    assert!(
        orch.playback.get_state(zone_id).await.volume <= 0.4,
        "l'état de lecture ne reste pas à 100 % : le pas des boutons en part"
    );
}

/// 🔴 avant / 🟢 après — PURE verrouillé : aucune consigne 100 %.
#[tokio::test]
async fn pure_verrouille_ne_suit_pas_la_zone_sur_un_autre_appareil() {
    aucune_consigne_a_100_sur_le_nouvel_appareil(Mode::PureVerrouille, "PURE verrouillé").await;
}

/// 🔴 avant / 🟢 après — volume fixe : aucune consigne 100 %.
#[tokio::test]
async fn le_volume_fixe_ne_suit_pas_la_zone_sur_un_autre_appareil() {
    aucune_consigne_a_100_sur_le_nouvel_appareil(Mode::VolumeFixe, "le volume fixe").await;
}

/// Témoin vert : le MÊME appareil garde PURE verrouillé — la graine part à
/// 100 % et la baisse reste refusée, comme avant.
#[tokio::test]
async fn le_meme_appareil_garde_pure_verrouille() {
    let (orch, zone_id) = zone_armee(Mode::PureVerrouille).await;
    let (recus, geste_accepte) = passer_sur(&orch, zone_id, ANCIEN).await;
    assert_eq!(
        recus,
        vec![1.0],
        "même appareil : PURE verrouillé reste armé"
    );
    assert!(
        !geste_accepte,
        "même appareil : la baisse reste refusée sous PURE verrouillé"
    );
}
