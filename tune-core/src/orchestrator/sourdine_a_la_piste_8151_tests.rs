//! 🔴 #8151 — de bout en bout : une zone en sourdine (`zones.muted`) confie
//! une piste à une sortie DLNA NEUVE, comme après un redémarrage du serveur.
//! La sortie ne sait rien de la sourdine ; l'appareil la lève à chaque URI.
//! Aucune note ne doit sortir. Aucun son réel : SOAP sur 127.0.0.1.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::dlna_sourdine_8151_tests::{FauxEversolo, demarrer, piste};
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

/// Monte la zone, sa sortie DLNA neuve et le faux appareil ; `muette` pose
/// la sourdine EN BASE seulement, comme un serveur qui vient de redémarrer.
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
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon 8151", Some("dlna"), Some(udn))
        .unwrap();
    ZoneRepo::with_backend(orch.db.clone())
        .update_muted(zone_id, muette)
        .unwrap();
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.30).await.unwrap();
    orch.outputs.lock().await.register(Box::new(sortie));
    etat.commandes.lock().await.clear();
    (orch, zone_id, etat, vie)
}

/// ⭐ Le cas de Bertrand, deux pistes de suite (musique puis podcast).
#[tokio::test]
async fn une_zone_dlna_en_sourdine_ne_repart_jamais_avec_le_son() {
    let udn = "uuid:banc-8151-zone";
    let (orch, zone_id, etat, vie) = banc(udn, true).await;

    for url in [
        "http://127.0.0.1:9/stream/8151-zone-a.flac",
        "http://127.0.0.1:9/stream/8151-zone-podcast.mp3",
    ] {
        let (envoyee, erreur) = orch
            .send_to_output(udn, &piste(url), None, false, zone_id, None)
            .await;
        assert!(envoyee, "la piste doit partir : {erreur:?}");
        assert_eq!(
            etat.audible().await,
            0,
            "#8151 : zone en sourdine repartie AVEC le son ({url}) ; reçu : {:?}",
            etat.commandes.lock().await
        );
    }
    assert_eq!(
        *etat.audible_au_play.lock().await,
        [0, 0],
        "chaque Play doit trouver l'appareil déjà muet ; reçu : {:?}",
        etat.commandes.lock().await
    );
    let commandes = etat.commandes.lock().await.clone();
    assert!(
        !commandes.iter().any(|c| c == "SetMute(0)"),
        "aucune commande ne doit lever la sourdine : {commandes:?}"
    );
    assert!(
        commandes.iter().any(|c| c == "SetMute(1)"),
        "la sourdine doit être réimposée : {commandes:?}"
    );

    // L'auditeur lève la sourdine : son volume revient.
    orch.set_mute(zone_id, false, Some(udn)).await.unwrap();
    assert_eq!(etat.audible().await, 30, "sourdine levée : volume rendu");

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Contre-épreuve : une zone qui n'est pas en sourdine ne reçoit aucun
/// `SetMute` de Tune au démarrage d'une piste.
#[tokio::test]
async fn une_zone_hors_sourdine_ne_recoit_aucune_sourdine() {
    let udn = "uuid:banc-8151-zone-sonore";
    let (orch, zone_id, etat, vie) = banc(udn, false).await;

    let (envoyee, erreur) = orch
        .send_to_output(
            udn,
            &piste("http://127.0.0.1:9/stream/8151-sonore.flac"),
            None,
            false,
            zone_id,
            None,
        )
        .await;
    assert!(envoyee, "la piste doit partir : {erreur:?}");
    let commandes = etat.commandes.lock().await.clone();
    assert!(
        !commandes.iter().any(|c| c.starts_with("SetMute")),
        "hors sourdine, aucun SetMute : {commandes:?}"
    );
    assert_eq!(etat.audible().await, 30);

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}
