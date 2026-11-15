//! #8165 — décision de Bertrand : un geste de volume sur une zone en sourdine
//! lève la sourdine, sur toutes les sorties. Banc sans son : faux appareil
//! DLNA sur 127.0.0.1 (celui de #8151), qui note sa sourdine et son volume.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::audio::garde_de_hausse::Hausse;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::dlna_sourdine_8151_tests::{FauxEversolo, demarrer};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

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

async fn banc(
    udn: &str,
    muette: bool,
) -> (
    PlaybackOrchestrator,
    i64,
    FauxEversolo,
    tokio::task::JoinHandle<()>,
) {
    compat::reinitialiser_compatibilite(udn).unwrap();
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon 8165", Some("dlna"), Some(udn))
        .unwrap();
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.40).await.unwrap();
    orch.outputs.lock().await.register(Box::new(sortie));
    orch.playback.set_volume(zone_id, 0.40).await;
    if muette {
        orch.set_mute(zone_id, true, Some(udn)).await.unwrap();
        assert_eq!(etat.audible().await, 0, "banc : la zone doit être muette");
    }
    etat.commandes.lock().await.clear();
    (orch, zone_id, etat, vie)
}

fn muette_en_base(orch: &PlaybackOrchestrator, zone_id: i64) -> bool {
    ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap()
        .muted
}

/// ⭐ Le cas de l'issue : zone coupée, l'auditeur baisse de 40 à 20 %.
#[tokio::test]
async fn un_geste_de_volume_leve_la_sourdine_8165() {
    let udn = "uuid:banc-8165-geste";
    let (orch, zone_id, etat, vie) = banc(udn, true).await;

    orch.set_volume_avec_garde(zone_id, 0.20, Some(udn), Hausse::Gardee)
        .await
        .unwrap();
    assert!(
        orch.lever_la_sourdine_au_geste_de_volume(zone_id, Some(udn))
            .await,
        "la zone était en sourdine"
    );

    assert_eq!(
        etat.audible().await,
        20,
        "#8165 : le geste lève la sourdine et le son revient au NOUVEAU volume ; reçu : {:?}",
        etat.commandes.lock().await
    );
    assert!(
        !muette_en_base(&orch, zone_id),
        "#8165 : la zone n'est plus en sourdine en base"
    );
    assert!(
        !orch.playback.get_state(zone_id).await.muted,
        "#8165 : ni dans l'état de lecture (l'écran le montre)"
    );

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Hors geste (sondeur, alarme, limite), `set_volume_avec_garde` seul garde
/// la sourdine : seules les routes de geste la lèvent.
#[tokio::test]
async fn un_volume_hors_geste_garde_la_sourdine_8165() {
    let udn = "uuid:banc-8165-hors-geste";
    let (orch, zone_id, etat, vie) = banc(udn, true).await;

    orch.set_volume_avec_garde(zone_id, 0.20, Some(udn), Hausse::Gardee)
        .await
        .unwrap();
    assert_eq!(etat.audible().await, 0, "{:?}", etat.commandes.lock().await);
    assert!(muette_en_base(&orch, zone_id));

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Une zone qui n'est pas en sourdine ne reçoit aucune commande de sourdine.
#[tokio::test]
async fn hors_sourdine_le_geste_n_envoie_aucune_sourdine_8165() {
    let udn = "uuid:banc-8165-sonore";
    let (orch, zone_id, etat, vie) = banc(udn, false).await;

    orch.set_volume_avec_garde(zone_id, 0.20, Some(udn), Hausse::Gardee)
        .await
        .unwrap();
    assert!(
        !orch
            .lever_la_sourdine_au_geste_de_volume(zone_id, Some(udn))
            .await
    );
    let commandes = etat.commandes.lock().await.clone();
    assert!(
        !commandes.iter().any(|c| c.starts_with("SetMute")),
        "hors sourdine, aucun SetMute : {commandes:?}"
    );
    assert_eq!(etat.audible().await, 20);

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Une zone sans sortie (navigateur, sortie absente) : la sourdine de la
/// zone se lève aussi.
#[tokio::test]
async fn sans_sortie_la_sourdine_de_la_zone_se_leve_8165() {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Navigateur 8165", Some("browser"), None)
        .unwrap();
    orch.set_mute(zone_id, true, None).await.unwrap();
    orch.set_volume_avec_garde(zone_id, 0.10, None, Hausse::Gardee)
        .await
        .unwrap();
    assert!(
        orch.lever_la_sourdine_au_geste_de_volume(zone_id, None)
            .await
    );
    assert!(!muette_en_base(&orch, zone_id));
    assert!(!orch.playback.get_state(zone_id).await.muted);
}
